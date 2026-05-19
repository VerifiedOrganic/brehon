//! Task tools for MCP.
//!
//! Tools for querying task context, listing tasks, and getting specific task details.

use async_trait::async_trait;
use brehon_ports::{EventStore, ProofStore, RunStore};
use brehon_types::{normalize_task_status, Event, EventId, EventKind, RunRecord, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::McpError;
use crate::tools::context_efficiency::{
    compact_text_if_enabled, load_context_tool_options, ContextToolOptions,
};
use crate::tools::freshness::ToolFreshness;
use crate::tools::proof_summary::ProofSummary;
use crate::tools::{error_result, text_result, Tool};

/// MCP tool that retrieves current task context including status and recent events.
pub struct GetTaskContextTool {
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
    proof_store: Option<Arc<dyn ProofStore + Send + Sync>>,
    run_store: Option<Arc<dyn RunStore + Send + Sync>>,
}

impl Default for GetTaskContextTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GetTaskContextTool {
    /// Create a new get-task-context tool instance.
    pub fn new() -> Self {
        Self {
            event_store: None,
            proof_store: None,
            run_store: None,
        }
    }

    /// Attach event store backing for recent events and revision metadata.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }

    /// Attach a proof projection so the returned `TaskContext` can include
    /// a compact proof summary alongside task state and recent events.
    pub fn with_proof_store(mut self, store: Arc<dyn ProofStore + Send + Sync>) -> Self {
        self.proof_store = Some(store);
        self
    }

    /// Attach durable run store backing for active run state.
    pub fn with_run_store(mut self, store: Arc<dyn RunStore + Send + Sync>) -> Self {
        self.run_store = Some(store);
        self
    }
}

#[async_trait]
impl Tool for GetTaskContextTool {
    fn name(&self) -> &str {
        "get_task_context"
    }

    fn description(&self) -> &str {
        "Get the current task context including description, status, and recent event history."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task ID to get context for. If not provided, returns context for current assigned task."
                },
                "event_limit": {
                    "type": "integer",
                    "description": "Maximum number of recent events to include",
                    "default": 5
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<crate::server::ToolResult, McpError> {
        let params: GetTaskContextParams =
            serde_json::from_value(args.clone()).unwrap_or(GetTaskContextParams {
                task_id: None,
                event_limit: None,
            });

        let options = load_context_tool_options();
        let event_limit = options.search_limit(params.event_limit);
        let mut context =
            match resolve_task_context(params.task_id.as_deref(), event_limit, &options)? {
                Some(context) => context,
                None => {
                    return Ok(error_result("No tasks found in runtime state"));
                }
            };

        let mut freshness = task_context_freshness(None, "runtime_task_file", &options, false);
        if let Some(event_store) = self.event_store.as_ref() {
            match durable_event_summaries(
                event_store.as_ref(),
                &context.task.id,
                event_limit,
                &options,
            )
            .await
            {
                Ok(snapshot) => {
                    if !snapshot.events.is_empty() {
                        context.events = snapshot.events;
                    }
                    context.source_event_id = snapshot.source_event_id;
                    context.truncated = snapshot.truncated;
                    freshness = task_context_freshness(
                        snapshot.source_event_id,
                        "event_store+runtime_task_file",
                        &options,
                        snapshot.truncated,
                    );
                }
                Err(err) => {
                    freshness = freshness
                        .stale(true)
                        .warning(format!("event_store_unavailable: {err}"));
                }
            }
        } else {
            freshness = freshness.stale(true).warning("event_store_unavailable");
        }

        context.active_run =
            resolve_active_run_summary(self.run_store.as_ref(), &context.task.id, &mut freshness)
                .await;
        context.review = resolve_review_summary(&context.task.id);

        let proof_summary =
            resolve_proof_summary(self.proof_store.as_ref(), &context.task.id).await;
        if let Some(summary) = proof_summary {
            context.proof_event_id = context
                .events
                .iter()
                .map(|event| event.event_id)
                .max()
                .filter(|value| *value != 0);
            context.proof = Some(summary);
        }
        context.freshness = freshness;

        let result_json = serde_json::to_string_pretty(&context)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

async fn resolve_proof_summary(
    proof_store: Option<&Arc<dyn ProofStore + Send + Sync>>,
    task_id: &str,
) -> Option<ProofSummary> {
    let store = proof_store?;
    match store.proof_bundle_for_task(&TaskId::new(task_id)).await {
        Ok(Some(bundle)) => Some(ProofSummary::from_bundle(&bundle)),
        Ok(None) => Some(ProofSummary::absent()),
        Err(err) => {
            tracing::warn!(
                task_id,
                error = %err,
                "Failed to load proof bundle for task context"
            );
            Some(ProofSummary::absent())
        }
    }
}

/// Input parameters for the `get_task_context` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct GetTaskContextParams {
    pub task_id: Option<String>,
    pub event_limit: Option<usize>,
}

/// Compact task representation with core fields used in context and list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
}

/// Summary of a single task event (e.g., assignment, prompt sent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSummary {
    pub event_id: u64,
    pub kind: String,
    pub timestamp: String,
    pub summary: String,
}

/// Full task context including the task summary, recent events, dependencies, and related files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContext {
    pub task: TaskSummary,
    pub events: Vec<EventSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_run: Option<RunContextSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewContextSummary>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub related_files: Vec<String>,
    #[serde(default)]
    pub research_context: Vec<Value>,
    /// Compact, bounded proof bundle summary for this task. Present when a
    /// proof store is wired, even if no bundle has been recorded yet (the
    /// summary will flag the absence explicitly).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<ProofSummary>,
    /// Revision marker derived from the highest recent event id. Helps
    /// agents tell whether the context is current or stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_event_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<u64>,
    pub generated_at: String,
    pub truncated: bool,
    pub freshness: ToolFreshness,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunContextSummary {
    pub run_id: String,
    pub role: String,
    pub status: String,
    pub attempt: u32,
    pub claim_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<String>,
    pub continuation_turns: u32,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewContextSummary {
    pub review_id: String,
    pub status: String,
    pub round: u32,
    pub panel_id: String,
    pub panel_progress: String,
    pub updated_at: String,
}

/// MCP tool that lists tasks with optional status filtering.
pub struct ListTasksTool;

impl Default for ListTasksTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListTasksTool {
    /// Create a new list-tasks tool instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ListTasksTool {
    fn name(&self) -> &str {
        "list_tasks"
    }

    fn description(&self) -> &str {
        "List tasks with optional filtering by status."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Filter by status: Pending, Assigned, InProgress, InReview, ChangesRequested, Approved, Merged, Blocked"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of tasks to return",
                    "default": 5
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<crate::server::ToolResult, McpError> {
        let params: ListTasksParams =
            serde_json::from_value(args.clone()).unwrap_or(ListTasksParams {
                status: None,
                limit: None,
            });

        let options = load_context_tool_options();
        let response = list_runtime_tasks(&params, &options)?;

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `list_tasks` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct ListTasksParams {
    pub status: Option<String>,
    pub limit: Option<usize>,
}

/// A task entry in the list-tasks response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskListItem {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
}

/// Response payload for the `list_tasks` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTasksResponse {
    pub tasks: Vec<TaskListItem>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_filter: Option<String>,
}

fn brehon_root_dir() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

fn tasks_dir() -> Option<PathBuf> {
    Some(brehon_root_dir()?.join("runtime").join("tasks"))
}

fn is_valid_task_id(task_id: &str) -> bool {
    if task_id.is_empty() {
        return false;
    }
    task_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn read_all_tasks() -> Vec<serde_json::Map<String, Value>> {
    let dir = match tasks_dir() {
        Some(dir) => dir,
        None => return Vec::new(),
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "json")
                && !entry.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|entry| {
            let content = std::fs::read_to_string(entry.path()).ok()?;
            serde_json::from_str::<Value>(&content).ok()
        })
        .filter_map(|value| value.as_object().cloned())
        .collect()
}

fn read_task_file(task_id: &str) -> Option<serde_json::Map<String, Value>> {
    let task_id = task_id.trim();
    if !is_valid_task_id(task_id) {
        return None;
    }

    let dir = tasks_dir()?;
    let path = dir.join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&content)
        .ok()?
        .as_object()
        .cloned()
}

fn read_string_field(task: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    task.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

fn read_string_list_field(task: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
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

fn normalize_priority(task: &serde_json::Map<String, Value>) -> String {
    match task.get("priority") {
        Some(Value::String(value)) => {
            if value.trim().is_empty() {
                "Medium".to_string()
            } else {
                value.to_string()
            }
        }
        Some(Value::Number(number)) => match number.as_u64() {
            Some(0) => "Critical".to_string(),
            Some(1) => "High".to_string(),
            Some(3) => "Low".to_string(),
            Some(4) => "Backlog".to_string(),
            _ => "Medium".to_string(),
        },
        _ => "Medium".to_string(),
    }
}

fn normalize_status(task: &serde_json::Map<String, Value>) -> String {
    read_string_field(task, "status")
        .and_then(|status| normalize_task_status(&status).map(str::to_string))
        .unwrap_or_else(|| "pending".to_string())
}

fn resolve_task_record(task_id: Option<&str>) -> Option<serde_json::Map<String, Value>> {
    if let Some(task_id) = task_id {
        return read_task_file(task_id);
    }

    let tasks = read_all_tasks();
    if tasks.is_empty() {
        return None;
    }

    if let Ok(agent_name) = std::env::var("BREHON_AGENT_NAME") {
        let trimmed = agent_name.trim();
        let mut assigned = tasks
            .iter()
            .filter(|task| read_string_field(task, "assignee").as_deref() == Some(trimmed))
            .collect::<Vec<_>>();
        if !assigned.is_empty() {
            assigned.sort_by(|left, right| {
                let left_updated = read_string_field(left, "updated_at").unwrap_or_default();
                let right_updated = read_string_field(right, "updated_at").unwrap_or_default();
                right_updated.cmp(&left_updated).then_with(|| {
                    read_string_field(left, "task_id").cmp(&read_string_field(right, "task_id"))
                })
            });
            if let Some(task) = assigned.into_iter().next() {
                return Some((*task).clone());
            }
        }
    }

    let mut tasks = tasks;
    tasks.sort_by(|left, right| {
        let left_updated = read_string_field(left, "updated_at").unwrap_or_default();
        let right_updated = read_string_field(right, "updated_at").unwrap_or_default();
        right_updated.cmp(&left_updated).then_with(|| {
            read_string_field(left, "task_id").cmp(&read_string_field(right, "task_id"))
        })
    });
    tasks.into_iter().next()
}

fn compact_task_context_text(content: String, options: &ContextToolOptions) -> String {
    if options.should_compact_tasks() {
        compact_text_if_enabled(&content, true, options.compression.mode)
    } else {
        content
    }
}

fn build_task_events(
    task: &serde_json::Map<String, Value>,
    event_limit: usize,
    options: &ContextToolOptions,
) -> Vec<EventSummary> {
    let Some(Value::Array(events)) = task.get("events") else {
        return Vec::new();
    };

    let mut parsed = events
        .iter()
        .filter_map(|event| {
            let event = event.as_object()?;

            Some(EventSummary {
                event_id: event
                    .get("event_id")
                    .and_then(|id| id.as_u64())
                    .unwrap_or(0),
                kind: event.get("kind").and_then(Value::as_str)?.to_string(),
                timestamp: event.get("timestamp").and_then(Value::as_str)?.to_string(),
                summary: compact_task_context_text(
                    event.get("summary").and_then(Value::as_str)?.to_string(),
                    options,
                ),
            })
        })
        .collect::<Vec<_>>();

    parsed.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| b.event_id.cmp(&a.event_id))
    });
    parsed.into_iter().take(event_limit).collect()
}

struct DurableEventSnapshot {
    events: Vec<EventSummary>,
    source_event_id: Option<u64>,
    truncated: bool,
}

fn task_context_freshness(
    source_event_id: Option<u64>,
    state_source: &str,
    options: &ContextToolOptions,
    truncated: bool,
) -> ToolFreshness {
    ToolFreshness::new(source_event_id, state_source)
        .compacted(options.should_compact_tasks())
        .truncated(truncated)
}

async fn durable_event_summaries(
    event_store: &(dyn EventStore + Send + Sync),
    task_id: &str,
    event_limit: usize,
    options: &ContextToolOptions,
) -> Result<DurableEventSnapshot, McpError> {
    const STREAM_LIMIT: usize = 10_000;
    let events = event_store
        .stream(None, STREAM_LIMIT)
        .await
        .map_err(|err| McpError::Storage(format!("Failed to stream task events: {err}")))?;
    let high_water = event_store
        .high_water_mark()
        .await
        .map_err(|err| McpError::Storage(format!("Failed to read event high-water mark: {err}")))?;
    let last_streamed = events.last().map(|(_, event_id)| event_id.as_u64());
    let truncated = last_streamed.is_some_and(|id| id < high_water.as_u64());
    let review_ids = review_ids_for_task(&events, task_id);
    let mut summaries = events
        .into_iter()
        .filter(|(event, _)| event_belongs_to_task(event, task_id, &review_ids))
        .map(|(event, event_id)| EventSummary {
            event_id: event_id.as_u64(),
            kind: event_kind_name(&event.kind).to_string(),
            timestamp: event.timestamp.to_rfc3339(),
            summary: compact_task_context_text(event_summary_text(&event), options),
        })
        .collect::<Vec<_>>();
    summaries.sort_by(|a, b| {
        b.event_id
            .cmp(&a.event_id)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
    });
    summaries.truncate(event_limit);
    Ok(DurableEventSnapshot {
        events: summaries,
        source_event_id: Some(high_water.as_u64()),
        truncated,
    })
}

fn review_ids_for_task(events: &[(Event, EventId)], task_id: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|(event, _)| match &event.kind {
            EventKind::ReviewRequested {
                task_id: event_task,
                review_id,
            } if event_task == task_id => Some(review_id.clone()),
            _ => None,
        })
        .collect()
}

fn event_belongs_to_task(event: &Event, task_id: &str, review_ids: &[String]) -> bool {
    event.kind.task_id() == Some(task_id)
        || event.aggregate_id == task_id
        || event
            .kind
            .review_id()
            .is_some_and(|review_id| review_ids.iter().any(|id| id == review_id))
}

fn event_kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::TaskCreated { .. } => "TaskCreated",
        EventKind::TaskAssigned { .. } => "TaskAssigned",
        EventKind::TaskCompleted { .. } => "TaskCompleted",
        EventKind::RunCreated { .. } => "RunCreated",
        EventKind::RunClaimed { .. } => "RunClaimed",
        EventKind::RunStarted { .. } => "RunStarted",
        EventKind::RunActivityObserved { .. } => "RunActivityObserved",
        EventKind::RunReleased { .. } => "RunReleased",
        EventKind::RunRetryQueued { .. } => "RunRetryQueued",
        EventKind::RunCompleted { .. } => "RunCompleted",
        EventKind::RunFailed { .. } => "RunFailed",
        EventKind::RunAbandoned { .. } => "RunAbandoned",
        EventKind::ReviewRequested { .. } => "ReviewRequested",
        EventKind::ReviewScoreReceived { .. } => "ReviewScoreReceived",
        EventKind::ReviewApproved { .. } => "ReviewApproved",
        EventKind::ReviewRejected { .. } => "ReviewRejected",
        EventKind::ReviewChangesRequested { .. } => "ReviewChangesRequested",
        EventKind::MergePrepared { .. } => "MergePrepared",
        EventKind::MergeCommitted { .. } => "MergeCommitted",
        EventKind::MergeAborted { .. } => "MergeAborted",
        _ => "Event",
    }
}

fn event_summary_text(event: &Event) -> String {
    match &event.kind {
        EventKind::TaskCreated { task_id } => format!("Task {task_id} created"),
        EventKind::TaskAssigned { task_id, agent_id } => {
            format!("Task {task_id} assigned to {agent_id}")
        }
        EventKind::TaskCompleted { task_id } => format!("Task {task_id} completed"),
        EventKind::RunCreated {
            run_id,
            role,
            status,
            ..
        } => format!("Run {run_id} created for {role} with status {status}"),
        EventKind::RunClaimed {
            run_id,
            owner,
            generation,
            ..
        } => format!("Run {run_id} claimed by {owner} at generation {generation}"),
        EventKind::RunStarted { run_id, role, .. } => {
            format!("Run {run_id} started for {role}")
        }
        EventKind::RunActivityObserved {
            run_id, activity, ..
        } => format!("Run {run_id} activity: {activity}"),
        EventKind::RunRetryQueued { run_id, reason, .. } => {
            format!("Run {run_id} queued for retry: {reason}")
        }
        EventKind::RunCompleted { run_id, .. } => format!("Run {run_id} completed"),
        EventKind::RunFailed { run_id, reason, .. } => {
            format!("Run {run_id} failed: {reason}")
        }
        EventKind::ReviewRequested { review_id, .. } => format!("Review {review_id} requested"),
        EventKind::ReviewScoreReceived {
            review_id,
            reviewer_id,
            score,
        } => format!("Review {review_id} received score {score} from {reviewer_id}"),
        EventKind::ReviewApproved { review_id } => format!("Review {review_id} approved"),
        EventKind::ReviewRejected { review_id } => format!("Review {review_id} rejected"),
        EventKind::ReviewChangesRequested { review_id } => {
            format!("Review {review_id} requested changes")
        }
        EventKind::MergePrepared { branch, .. } => format!("Merge prepared on {branch}"),
        EventKind::MergeCommitted { task_id } => format!("Merge committed for {task_id}"),
        EventKind::MergeAborted { task_id, reason } => {
            format!("Merge aborted for {task_id}: {reason}")
        }
        _ => event_kind_name(&event.kind).to_string(),
    }
}

async fn resolve_active_run_summary(
    run_store: Option<&Arc<dyn RunStore + Send + Sync>>,
    task_id: &str,
    freshness: &mut ToolFreshness,
) -> Option<RunContextSummary> {
    let Some(store) = run_store else {
        freshness.warnings.push("run_store_unavailable".to_string());
        return None;
    };
    match store.runs_for_task(&TaskId::new(task_id)).await {
        Ok(mut runs) => {
            runs.retain(RunRecord::is_active);
            runs.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
            runs.into_iter().next().map(run_summary)
        }
        Err(err) => {
            freshness.stale = true;
            freshness
                .warnings
                .push(format!("run_store_unavailable: {err}"));
            None
        }
    }
}

fn run_summary(run: RunRecord) -> RunContextSummary {
    RunContextSummary {
        run_id: run.run_id.to_string(),
        role: run.role.to_string(),
        status: run.status.to_string(),
        attempt: run.attempt,
        claim_generation: run.claim_generation.as_u64(),
        claim_owner: run.claim_owner.map(|owner| owner.to_string()),
        session_id: run.session_id.map(|session| session.to_string()),
        lease_expires_at: run.lease_expires_at.map(|time| time.to_rfc3339()),
        continuation_turns: run.continuation_turns,
        updated_at: run.updated_at.to_rfc3339(),
    }
}

fn resolve_review_summary(task_id: &str) -> Option<ReviewContextSummary> {
    let state = crate::tools::verification::read_review_state(task_id)?;
    Some(ReviewContextSummary {
        review_id: state.current_review_id,
        status: state.status,
        round: state.current_round,
        panel_id: state.panel_id,
        panel_progress: format!("{}/{}", state.submissions_received.len(), state.panel.len()),
        updated_at: state.updated_at,
    })
}

fn resolve_task_context(
    task_id: Option<&str>,
    event_limit: usize,
    options: &ContextToolOptions,
) -> Result<Option<TaskContext>, McpError> {
    let task = if let Some(task_id) = task_id {
        let task_id = task_id.trim();
        if task_id.is_empty() {
            return Err(McpError::InvalidRequest(
                "get_task_context requires a non-empty task_id".to_string(),
            ));
        }
        if !is_valid_task_id(task_id) {
            return Err(McpError::InvalidRequest(format!(
                "Invalid task_id: {task_id}"
            )));
        }

        match read_task_file(task_id) {
            Some(task) => task,
            None => {
                return Err(McpError::InvalidRequest(format!(
                    "Task not found: {task_id}"
                )))
            }
        }
    } else {
        match resolve_task_record(None) {
            Some(task) => task,
            None => return Ok(None),
        }
    };

    let Some(task_id) = read_string_field(&task, "task_id").or_else(|| task_id.map(str::to_string))
    else {
        return Err(McpError::InvalidRequest(
            "Task snapshot is missing task_id".to_string(),
        ));
    };

    Ok(Some(TaskContext {
        task: TaskSummary {
            id: task_id,
            title: read_string_field(&task, "title").unwrap_or_else(|| "Untitled task".to_string()),
            description: compact_task_context_text(
                read_string_field(&task, "description").unwrap_or_default(),
                options,
            ),
            status: normalize_status(&task),
            priority: normalize_priority(&task),
            assignee: read_string_field(&task, "assignee"),
        },
        events: build_task_events(&task, event_limit, options),
        active_run: None,
        review: None,
        dependencies: read_string_list_field(&task, "dependencies"),
        related_files: read_string_list_field(&task, "file_hints"),
        research_context: task
            .get("research_context")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        proof: None,
        proof_event_id: None,
        source_event_id: None,
        generated_at: chrono::Utc::now().to_rfc3339(),
        truncated: false,
        freshness: task_context_freshness(None, "runtime_task_file", options, false),
    }))
}

fn list_runtime_tasks(
    params: &ListTasksParams,
    options: &ContextToolOptions,
) -> Result<ListTasksResponse, McpError> {
    let limit = options.search_limit(params.limit);
    let normalized_status = params
        .status
        .as_deref()
        .and_then(|status| normalize_task_status(status));

    let mut tasks = read_all_tasks();
    if params.status.is_some() {
        let Some(status_filter) = normalized_status else {
            return Ok(ListTasksResponse {
                tasks: Vec::new(),
                count: 0,
                status_filter: None,
            });
        };
        tasks.retain(|task| normalize_status(task).as_str() == status_filter);
    }

    tasks.sort_by(|left, right| {
        let left_updated = read_string_field(left, "updated_at").unwrap_or_default();
        let right_updated = read_string_field(right, "updated_at").unwrap_or_default();
        right_updated.cmp(&left_updated).then_with(|| {
            read_string_field(left, "task_id").cmp(&read_string_field(right, "task_id"))
        })
    });

    let tasks = tasks
        .into_iter()
        .take(limit)
        .map(|task| TaskListItem {
            id: read_string_field(&task, "task_id").unwrap_or_else(|| "(untitled)".to_string()),
            title: read_string_field(&task, "title").unwrap_or_else(|| "Untitled task".to_string()),
            status: normalize_status(&task),
            priority: normalize_priority(&task),
            assignee: read_string_field(&task, "assignee"),
        })
        .collect::<Vec<_>>();
    let count = tasks.len();

    Ok(ListTasksResponse {
        tasks,
        count,
        status_filter: params.status.clone(),
    })
}

/// MCP tool that retrieves detailed information about a specific task by ID.
pub struct GetTaskTool;

impl Default for GetTaskTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GetTaskTool {
    /// Create a new get-task tool instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for GetTaskTool {
    fn name(&self) -> &str {
        "get_task"
    }

    fn description(&self) -> &str {
        "Get detailed information about a specific task."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task ID to retrieve"
                }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<crate::server::ToolResult, McpError> {
        let params: GetTaskParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let task_id = params.task_id.trim();
        if task_id.is_empty() {
            return Ok(error_result("Task ID cannot be empty"));
        }
        if !is_valid_task_id(task_id) {
            return Ok(error_result(format!("Invalid task_id: {task_id}")));
        }

        let Some(task_record) = read_task_file(task_id) else {
            return Ok(error_result(format!("Task not found: {task_id}")));
        };

        let task = read_task_detail(task_id, &task_record);
        let result_json = serde_json::to_string_pretty(&task)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

fn read_task_detail(task_id: &str, task: &serde_json::Map<String, Value>) -> TaskDetail {
    let resolved_id = read_string_field(task, "task_id").unwrap_or_else(|| task_id.to_string());
    let now = chrono::Utc::now().to_rfc3339();
    TaskDetail {
        id: resolved_id,
        title: read_string_field(task, "title").unwrap_or_else(|| "Untitled task".to_string()),
        description: read_string_field(task, "description").unwrap_or_default(),
        status: normalize_status(task),
        priority: normalize_priority(task),
        assignee: read_string_field(task, "assignee"),
        dependencies: read_string_list_field(task, "dependencies"),
        created_at: read_string_field(task, "created_at").unwrap_or_else(|| now.clone()),
        updated_at: read_string_field(task, "updated_at").unwrap_or(now),
        notes: build_task_notes(task),
    }
}

fn build_task_notes(task: &serde_json::Map<String, Value>) -> Vec<TaskNote> {
    let Some(Value::Array(events)) = task.get("events") else {
        return Vec::new();
    };
    events
        .iter()
        .filter_map(|event| {
            let event = event.as_object()?;
            Some(TaskNote {
                author: event
                    .get("author")
                    .and_then(Value::as_str)
                    .unwrap_or("system")
                    .to_string(),
                kind: event.get("kind").and_then(Value::as_str)?.to_string(),
                content: event
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                created_at: event
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Input parameters for the `get_task` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct GetTaskParams {
    pub task_id: String,
}

/// Full task details including metadata, dependencies, timestamps, and notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub notes: Vec<TaskNote>,
}

/// A timestamped note attached to a task by an author.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNote {
    pub author: String,
    pub kind: String,
    pub content: String,
    pub created_at: String,
}

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tasks_tests;

#[cfg(test)]
#[path = "tasks_proof_tests.rs"]
mod tasks_proof_tests;
