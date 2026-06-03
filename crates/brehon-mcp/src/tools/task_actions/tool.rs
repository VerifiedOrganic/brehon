//! TaskActionsTool: the MCP tool struct, schema, and execute dispatch.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{error_result, Tool};
use brehon_ports::{EventStore, ProofStore};

use super::integration_proof::IntegrationProofRecorder;
use super::proof::WorkerProofRecorder;

/// MCP tool for task lifecycle management: create, list, progress, close, and more.
pub struct TaskActionsTool {
    proof_recorder: WorkerProofRecorder,
    integration_proof_recorder: IntegrationProofRecorder,
}

#[allow(non_upper_case_globals)]
/// Backward-compatible value for tests and callers that used the old unit struct constructor.
pub const TaskActionsTool: TaskActionsTool = TaskActionsTool {
    proof_recorder: WorkerProofRecorder::empty(),
    integration_proof_recorder: IntegrationProofRecorder::empty(),
};

impl Default for TaskActionsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskActionsTool {
    /// Create a new task-actions tool instance.
    pub fn new() -> Self {
        super::migration::migrate_legacy_integration_conflicts_in_tasks_dir();
        super::migration::restore_nulled_assignees_in_tasks_dir();
        Self {
            proof_recorder: WorkerProofRecorder::default(),
            integration_proof_recorder: IntegrationProofRecorder::default(),
        }
    }

    /// Attach the event stream used for durable proof evidence.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.proof_recorder = self.proof_recorder.with_event_store(store.clone());
        self.integration_proof_recorder = self.integration_proof_recorder.with_event_store(store);
        self
    }

    /// Attach the proof projection used to query and update worker evidence.
    pub fn with_proof_store(mut self, store: Arc<dyn ProofStore + Send + Sync>) -> Self {
        self.proof_recorder = self.proof_recorder.with_proof_store(store.clone());
        self.integration_proof_recorder = self.integration_proof_recorder.with_proof_store(store);
        self
    }
}

#[async_trait]
impl Tool for TaskActionsTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Task lifecycle management. Supervisors should call action=ready and follow its next_action before guessing workflow steps; workers use checkpoint/complete/progress, supervisors use repair_frontier/recover_handoff/request_review/integrate/close recovery paths."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: list, mine, ready, repair_frontier, recover_handoff, checkpoint, complete, close, archive, integrate, abort-integration, progress, update, create, subtasks, children, conflicts, followups, promote_followups, waive_followups, ensure_final_hardening. Supervisors: call ready first; ready returns priority queues plus next_action. If next_action.kind=repair_frontier or recover_handoff, call that exact task action instead of guessing status updates."
                },
                "id": {
                    "type": "string",
                    "description": "Task ID"
                },
                "task_type": {
                    "type": "string",
                    "description": "Task type: initiative, epic, or task (default: task)"
                },
                "parent_id": {
                    "type": "string",
                    "description": "Parent task ID. Initiatives can own epics; epics can own worker tasks."
                },
                "dependencies": {
                    "type": "array",
                    "description": "Task IDs that must be terminal before this task can become pending or assignable",
                    "items": { "type": "string" }
                },
                "status": {
                    "type": "string",
                    "description": "Task status or filter. Do not guess status transitions. Workers finish with action=complete; supervisors start reviews with verification action=request_review. Use recover_handoff or repair_frontier for blocked worker handoff recovery; supervisor action=update status=review_ready is retained only for backward-compatible integration-conflict recovery."
                },
                "include_closed": {
                    "type": "boolean",
                    "description": "If true, include closed tasks in list results (default: false)"
                },
                "include_assignment_observability": {
                    "type": "boolean",
                    "description": "For action=list, include assignment_observability by resolving prompt-delivery and pane-context runtime state. Defaults to false to keep list queries on the hot path lightweight."
                },
                "title": {
                    "type": "string",
                    "description": "Task title (for create)"
                },
                "description": {
                    "type": "string",
                    "description": "Task description"
                },
                "acceptance_criteria": {
                    "type": "array",
                    "description": "Structured acceptance criteria for epics and implementation subtasks",
                    "items": { "type": "string" }
                },
                "file_hints": {
                    "type": "array",
                    "description": "Files, modules, or areas the worker should focus on",
                    "items": { "type": "string" }
                },
                "constraints": {
                    "type": "array",
                    "description": "Design or execution constraints the worker must respect",
                    "items": { "type": "string" }
                },
                "test_requirements": {
                    "type": "array",
                    "description": "Tests or validation steps required before review",
                    "items": { "type": "string" }
                },
                "plan_steps": {
                    "type": "array",
                    "description": "Expected implementation plan or phase breakdown",
                    "items": { "type": "string" }
                },
                "implementation_notes": {
                    "type": "string",
                    "description": "Extra implementation guidance beyond the summary"
                },
                "completion_mode": {
                    "type": "string",
                    "description": "Terminal behavior for approved tasks: merge or close"
                },
                "priority": {
                    "type": "string",
                    "description": "Priority: low, medium, high, critical"
                },
                "execution_policy": {
                    "type": "object",
                    "description": "Preferred execution lane/model metadata for assignment decisions, e.g. work_class, preferred_lane, preferred_agent_type, preferred_model, preferred_reasoning_effort, strict."
                },
                "source_file": {
                    "type": "string",
                    "description": "For action=ensure_final_hardening: optional source plan path to persist in plan_import metadata."
                },
                "percent": {
                    "type": "integer",
                    "description": "Progress percentage"
                },
                "notes": {
                    "type": "string",
                    "description": "Progress notes"
                },
                "message": {
                    "type": "string",
                    "description": "Checkpoint commit message for action=checkpoint, or optional commit-message override for action=complete"
                },
                "activity": {
                    "type": "string",
                    "description": "Current activity"
                },
                "blockers": {
                    "type": "string",
                    "description": "Blocker description. For blocked tasks, action=ready classifies recoverable worker handoff blockers separately and may return a next_action to recover them."
                },
                "agent_name": {
                    "type": "string",
                    "description": "Caller agent name override for notifications/tests"
                },
                "role": {
                    "type": "string",
                    "description": "Caller role override for transition checks/tests"
                },
                "supervisor": {
                    "type": "string",
                    "description": "Supervisor name override for notifications/tests"
                },
                "integration_branch": {
                    "type": "string",
                    "description": "For initiatives and implementation epics: the integration branch name. Initiatives default to an initiative/* branch. Epics default to an epic/* branch unless direct_to_main=true."
                },
                "integration_worktree": {
                    "type": "string",
                    "description": "Read-only for initiatives and implementation epics: the dedicated integration worktree path where the integration branch is checked out."
                },
                "merge_target": {
                    "type": "string",
                    "description": "For merge-mode subtasks: the branch they merge into. Feature-epic subtasks inherit the parent integration_branch. Plain epics require direct_to_main=true instead of silently defaulting to main."
                },
                "integration_status": {
                    "type": "string",
                    "description": "For subtasks under feature epics: pending, integrated, or not_applicable"
                },
                "direct_to_main": {
                    "type": "boolean",
                    "description": "Explicitly opt into direct-to-default-branch flow. Use on an implementation epic to keep it plain, or on a merge-mode subtask under a plain epic."
                },
                "reason": {
                    "type": "string",
                    "description": "For action=archive or abort-integration: human-readable reason for the change"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "For action=archive: archive descendants too when targeting an epic or initiative"
                },
                "followup_ids": {
                    "type": "array",
                    "description": "For followup actions: specific followup_ids to act on. Defaults to all open followups.",
                    "items": { "type": "string" }
                },
                "include_resolved_followups": {
                    "type": "boolean",
                    "description": "For action=followups: include tasked/waived/done followups in addition to open ones."
                },
                "followup_title": {
                    "type": "string",
                    "description": "For action=promote_followups: optional title override for the created followup task."
                },
                "waive_all": {
                    "type": "boolean",
                    "description": "For action=waive_followups: required when waiving every open followup without passing explicit followup_ids. When multiple followups are open, the action rejects blanket waives unless this is true and a reason is provided."
                },
                "force": {
                    "type": "boolean",
                    "description": "For action=integrate: supervisor-only escape hatch. If the integration state is Aborted (or CherryPicking/Resolved with unrecoverable git state), force=true discards prior integration state and starts fresh. The prior phase is logged via tracing::warn!. force=true does NOT re-run an already-Complete integration — that requires a manual revert first."
                }
            },
            "required": ["action"],
            "examples": [
                {
                    "action": "ready"
                },
                {
                    "action": "repair_frontier"
                },
                {
                    "action": "recover_handoff",
                    "id": "T-example"
                },
                {
                    "action": "complete",
                    "id": "T-example",
                    "notes": "Implementation complete",
                    "activity": "testing"
                },
                {
                    "action": "integrate",
                    "id": "T-example"
                }
            ]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        match action {
            "create" => super::action_create::execute(&args).await,
            "list" => super::action_query::execute_list(&args).await,
            "followups" => super::action_followup::execute_followups(&args).await,
            "promote_followups" => super::action_followup::execute_promote(&args).await,
            "waive_followups" => super::action_followup::execute_waive(&args).await,
            "ensure_final_hardening" => super::final_hardening::execute_ensure(&args).await,
            "mine" => super::action_query::execute_mine(&args).await,
            "conflicts" => super::action_query::execute_conflicts(&args).await,
            "ready" => super::action_query::execute_ready(&args).await,
            "repair_frontier" => super::action_repair::execute_repair_frontier(&args).await,
            "recover_handoff" => super::action_repair::execute_recover_handoff(&args).await,
            "children" | "subtasks" => super::action_query::execute_children(&args).await,
            "integrate" => {
                super::action_integrate::execute(&args, &self.integration_proof_recorder).await
            }
            "abort-integration" => {
                super::action_abort_integration::execute(&args, &self.integration_proof_recorder)
                    .await
            }
            "close" => super::action_close::execute(&args).await,
            "archive" => super::action_update::execute_archive(&args).await,
            "checkpoint" => {
                super::action_update::execute_checkpoint(&args, &self.proof_recorder).await
            }
            "complete" => super::action_update::execute_complete(&args, &self.proof_recorder).await,
            "progress" => super::action_update::execute_progress(&args, &self.proof_recorder).await,
            "update" => super::action_update::execute_update(&args, &self.proof_recorder).await,
            _ => Ok(error_result(format!("Unknown task action: {action}"))),
        }
    }
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "proof_tests.rs"]
mod proof_tests;
