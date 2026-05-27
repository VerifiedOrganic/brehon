//! VerificationTool: the MCP tool struct, schema, constructors, sweep, and dispatch.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use brehon_ports::{EventStore, ProofStore};
use brehon_review::scoring::{ScoreCollector, ThresholdEvaluator};
use brehon_review::FeedbackConsolidator;
use brehon_review::PriorityQueue;
use brehon_types::config::{ReviewConfig, ReviewLeaseMode, ReviewPanelConfig, ReviewPanelMode};
use brehon_types::{
    normalize_task_status, Event, EventKind, Priority, ReviewFinding, ReviewScore,
    TaskCompletionMode,
};

use crate::error::McpError;
use crate::server::{ContentBlock, ToolResult};
use crate::tools::agent::try_deliver_message;
use crate::tools::assignment_observability::AssignmentPropagation;
use crate::tools::{error_result, text_result, Tool};

use super::helpers::{brehon_root, current_git_head_short, reviews_dir, workspace_root};
use super::maintenance::{PanelReassignmentResult, ReviewMaintenanceAction};
use super::notifications::{notify_review_stakeholders, reviewer_reset_ack_exists};
use super::panel::{
    build_full_council_panel, build_panel, find_agents_by_role, find_agents_by_role_with_type,
    find_panel_lease_by_task, read_all_panel_leases, read_panel_seat, release_panel_lease_for_task,
    write_panel_lease, AgentInfo, PanelLeaseMember, PanelLeaseState, PanelReviewerReplacement,
    IMPLICIT_PANEL_ID,
};
use super::review_prompt::{build_review_request_prompt, ReviewRequestPromptInput};
use super::scoring::{
    build_task_review_feedback, build_task_review_followups, task_status_for_review_outcome,
    unsupported_negative_review_reason,
};
use super::state::{
    acquire_review_lock, current_review_cycle_round, delete_review_state, parse_verdict,
    read_review_state, read_round_submissions, round_dir, total_review_round_limit,
    total_review_rounds_exhausted, write_consolidated, write_review_state, write_round_request,
    ConsolidatedReport, ReviewRequestFile, ReviewState, StoredCalibration, StoredCalibrationEntry,
    StoredFinding, StoredSubmission,
};
use super::tasks::{
    detect_default_branch, merge_target_requires_epic_integration, read_task,
    read_task_completion_mode, read_task_merge_target,
};

use crate::tools::task_actions::{
    acquire_task_lock, append_task_review_followups, release_task_worker_to_review,
    set_task_review_feedback, update_task_status_atomic,
};

// Items used only in tests (via `super::*` from tool_tests.rs)
#[cfg(test)]
use super::state::{read_round_request, verdict_str};
#[cfg(test)]
use brehon_types::{CommentSeverity, ReviewVerdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RejectionReason {
    MissingReviewState,
    TaskClosed,
    RoundSuperseded,
    // Reserved for review fingerprint/commit validation once submit_review has
    // a reachable commit-mismatch branch.
    #[allow(dead_code)]
    CommitMismatch,
    UnknownReviewId,
}

impl RejectionReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::MissingReviewState => "missing_review_state",
            Self::TaskClosed => "task_closed",
            Self::RoundSuperseded => "round_superseded",
            Self::CommitMismatch => "commit_mismatch",
            Self::UnknownReviewId => "unknown_review_id",
        }
    }
}

/// MCP tool for multi-reviewer code verification: request, submit, and consolidate reviews.
pub struct VerificationTool {
    pub(super) config: ReviewConfig,
    pub(super) event_store: Option<Arc<dyn EventStore + Send + Sync>>,
    pub(super) proof_recorder: super::proof::ReviewProofRecorder,
}

fn task_status_for_report(state: &ReviewState, report: &ConsolidatedReport) -> &'static str {
    if report.outcome == "escalated" && total_review_rounds_exhausted(state) {
        "blocked"
    } else {
        task_status_for_review_outcome(&report.outcome).unwrap_or("changes_requested")
    }
}

impl Default for VerificationTool {
    fn default() -> Self {
        Self::new()
    }
}

impl VerificationTool {
    pub(super) fn share_after_submit_enabled(&self) -> bool {
        self.config.lease_mode == ReviewLeaseMode::ShareAfterSubmit
    }

    fn lease_without_active_round_still_reserves_members(task_id: &str) -> bool {
        let Some(task) = read_task(task_id) else {
            return false;
        };
        matches!(
            normalize_task_status(
                task.get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("pending"),
            ),
            Some("in_review")
        )
    }

    fn review_state_is_superseded_by_task_status(
        task_status: Option<&str>,
        state: &ReviewState,
    ) -> bool {
        let normalized = task_status.and_then(normalize_task_status);
        if matches!(normalized, Some("merged" | "closed")) {
            return true;
        }

        match state.status.as_str() {
            "collecting" => matches!(normalized, Some("merged" | "closed")),
            "changes_requested" | "released" | "rejected" | "escalated" => matches!(
                normalized,
                Some("review_ready" | "in_progress" | "assigned")
            ),
            "approved" => matches!(
                normalized,
                Some("review_ready" | "in_progress" | "assigned" | "changes_requested")
            ),
            _ => false,
        }
    }

    async fn release_superseded_review_state(
        &self,
        task_id: &str,
        requested_by: &str,
    ) -> Result<Option<ReviewMaintenanceAction>, String> {
        let task_status = read_task(task_id)
            .and_then(|task| {
                task.get("status")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());

        let _lock = match acquire_review_lock(task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Err(format!(
                    "Failed to lock review state for task {task_id} during stale review cleanup: {err}"
                ));
            }
        };
        let Some(state) = read_review_state(task_id) else {
            return Ok(None);
        };

        if !Self::review_state_is_superseded_by_task_status(Some(task_status.as_str()), &state) {
            if state.status == "collecting"
                && normalize_task_status(task_status.as_str()) != Some("in_review")
            {
                update_task_status_atomic(task_id, "in_review").await?;
                release_task_worker_to_review(task_id, None).await?;
            }
            return Ok(None);
        }

        if state.status == "collecting" {
            let cancellation_message = format!(
                "Review {} for task {task_id} is no longer active. The task is now {}. Stop reviewing this round. A fresh review request will arrive with a new review_id if needed.",
                state.current_review_id, task_status
            );
            self.notify_pending_panel_reviewers(&state, requested_by, &cancellation_message);
        }

        let _ = release_panel_lease_for_task(task_id)?;
        delete_review_state(task_id).map_err(|err| {
            format!("Failed to delete stale review state for task {task_id}: {err}")
        })?;

        Ok(Some(ReviewMaintenanceAction::ReleasedStaleReviewState {
            task_id: task_id.to_string(),
            review_id: state.current_review_id,
            review_status: state.status,
            task_status,
        }))
    }

    fn active_reserved_members(&self, lease: &PanelLeaseState) -> Vec<PanelLeaseMember> {
        if !self.share_after_submit_enabled() {
            return lease.members.clone();
        }

        let Some(state) = read_review_state(&lease.task_id) else {
            return if Self::lease_without_active_round_still_reserves_members(&lease.task_id) {
                lease.members.clone()
            } else {
                Vec::new()
            };
        };
        if state.current_review_id != lease.review_id {
            return if Self::lease_without_active_round_still_reserves_members(&lease.task_id) {
                lease.members.clone()
            } else {
                Vec::new()
            };
        }

        // Per-member reservation: a member is "released" once they have
        // submitted AND a reset-ack exists for them. The previous configured-
        // panels branch used all-or-nothing semantics (panel only counted as
        // clear when *every* member was acked), which kept the still-pending
        // members blocking the released ones — and meant a partially-released
        // panel could never be reused even when free reviewers were available
        // to fill the remaining slots. The behaviour here now matches the
        // implicit-panel branch.
        lease
            .members
            .iter()
            .filter(|member| {
                !state.submissions_received.contains(&member.reviewer)
                    || !reviewer_reset_ack_exists(
                        &lease.task_id,
                        &lease.review_id,
                        &member.reviewer,
                    )
            })
            .cloned()
            .collect()
    }

    fn reserved_reviewers_for_active_leases(
        &self,
        active_leases: &[PanelLeaseState],
        exclude_task: Option<&str>,
    ) -> HashSet<String> {
        active_leases
            .iter()
            .filter(|lease| exclude_task.is_none_or(|task_id| lease.task_id != task_id))
            .flat_map(|lease| self.active_reserved_members(lease).into_iter())
            .map(|member| member.reviewer)
            .collect()
    }

    fn runtime_tasks_dir() -> Option<PathBuf> {
        brehon_root().map(|root| root.join("runtime").join("tasks"))
    }

    fn task_result_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn write_task_json(task_id: &str, task: &Value) -> Result<(), String> {
        let path = Self::runtime_tasks_dir()
            .ok_or_else(|| "BREHON_ROOT is not configured".to_string())?
            .join(format!("{task_id}.json"));
        let parent = path
            .parent()
            .ok_or_else(|| format!("Invalid task path for {task_id}"))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create task dir for {task_id}: {err}"))?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("Invalid task filename for {task_id}"))?;
        let temp_path = parent.join(format!(".{file_name}.tmp"));
        std::fs::write(
            &temp_path,
            serde_json::to_string_pretty(task)
                .map_err(|err| format!("Failed to serialize task {task_id}: {err}"))?,
        )
        .map_err(|err| format!("Failed to write temp task file for {task_id}: {err}"))?;
        std::fs::rename(&temp_path, &path)
            .map_err(|err| format!("Failed to persist task {task_id}: {err}"))?;
        Ok(())
    }

    fn maintenance_request_description(task: &Value) -> String {
        let raw = task
            .get("notes")
            .and_then(|value| value.as_str())
            .or_else(|| task.get("description").and_then(|value| value.as_str()))
            .unwrap_or("")
            .trim();
        const LIMIT: usize = 2000;
        if raw.len() <= LIMIT {
            raw.to_string()
        } else {
            format!("{}...", raw.chars().take(LIMIT).collect::<String>())
        }
    }

    async fn recover_orphaned_review_gate_task(
        &self,
        task_id: &str,
    ) -> Result<Option<ReviewMaintenanceAction>, String> {
        if read_review_state(task_id).is_some() {
            return Ok(None);
        }

        let _lock = acquire_task_lock(task_id).await?;
        let Some(mut task) = read_task(task_id) else {
            return Ok(None);
        };

        let current_status = task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("pending");
        if normalize_task_status(current_status) != Some("in_review") {
            return Ok(None);
        }

        let blockers = task
            .get("blockers")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let review_feedback_outcome = task
            .get("review_feedback")
            .and_then(|value| value.get("outcome"))
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let should_restore_revision = review_feedback_outcome == "changes_requested"
            || blockers.contains("does not integrate cleanly")
            || blockers.contains("checkpoint again")
            || blockers.contains("re-request review")
            || blockers.contains("resubmit");
        let live_workers: std::collections::HashSet<String> =
            find_agents_by_role("worker").into_iter().collect();

        let repaired_status = if should_restore_revision {
            if task
                .get("assignee")
                .and_then(|value| value.as_str())
                .is_none()
            {
                if let Some(review_owner) = task
                    .get("review_owner")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .filter(|value| live_workers.contains(*value))
                {
                    task["assignee"] = serde_json::json!(review_owner);
                }
            }
            "changes_requested"
        } else {
            if task
                .get("assignee")
                .and_then(|value| value.as_str())
                .is_none()
            {
                if let Some(review_owner) = task
                    .get("review_owner")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .filter(|value| live_workers.contains(*value))
                {
                    task["assignee"] = serde_json::json!(review_owner);
                }
            }
            "review_ready"
        };

        task["status"] = serde_json::json!(repaired_status);
        task["updated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
        Self::write_task_json(task_id, &task)?;

        Ok(Some(ReviewMaintenanceAction::RecoveredOrphanedGate {
            task_id: task_id.to_string(),
            from_status: "in_review".to_string(),
            to_status: repaired_status.to_string(),
        }))
    }

    async fn release_dead_changes_requested_assignee(
        &self,
        task_id: &str,
    ) -> Result<Option<ReviewMaintenanceAction>, String> {
        let _lock = acquire_task_lock(task_id).await?;
        let Some(mut task) = read_task(task_id) else {
            return Ok(None);
        };

        if normalize_task_status(
            task.get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("pending"),
        ) != Some("changes_requested")
        {
            return Ok(None);
        }

        let Some(assignee) = task
            .get("assignee")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
        else {
            return Ok(None);
        };

        let live_workers: HashSet<String> = find_agents_by_role("worker").into_iter().collect();
        if live_workers.contains(&assignee) {
            return Ok(None);
        }

        task["orphaned_assignee"] = serde_json::json!(assignee);
        task["orphaned_status"] = serde_json::json!("changes_requested");
        task["assignee"] = Value::Null;
        task["inbox_delivered"] = serde_json::json!(false);
        task.as_object_mut().map(|object| object.remove("activity"));
        task["updated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
        Self::write_task_json(task_id, &task)?;

        Ok(Some(
            ReviewMaintenanceAction::ReleasedDeadWorkerAssignment {
                task_id: task_id.to_string(),
                status: "changes_requested".to_string(),
                previous_assignee: assignee,
            },
        ))
    }

    async fn auto_request_review_ready_task(
        &self,
        task_id: &str,
        requested_by: &str,
    ) -> Result<Option<ReviewMaintenanceAction>, String> {
        if read_review_state(task_id).is_some() {
            return Ok(None);
        }

        let Some(task) = read_task(task_id) else {
            return Ok(None);
        };
        if normalize_task_status(
            task.get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("pending"),
        ) != Some("review_ready")
        {
            return Ok(None);
        }
        if matches!(
            task.get("task_type").and_then(|value| value.as_str()),
            Some("epic" | "initiative")
        ) {
            return Ok(None);
        }

        let completion_mode = read_task_completion_mode(task_id);
        let commit = task
            .get("latest_commit")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from);
        if completion_mode == TaskCompletionMode::Merge && commit.is_none() {
            return Err(format!(
                "Task {task_id} is review_ready but has no latest_commit recorded"
            ));
        }

        let mut request = serde_json::json!({
            "task_id": task_id,
            "title": task
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("(untitled)"),
            "description": Self::maintenance_request_description(&task),
            "requested_by": requested_by,
        });
        if let Some(commit) = commit {
            request["commit"] = serde_json::json!(commit);
        }

        let result = self
            .handle_request_review(&request)
            .await
            .map_err(|err| format!("Failed to auto-request review for {task_id}: {err}"))?;
        if result.is_error == Some(true) {
            return Err(format!(
                "Auto-request review failed for {task_id}: {}",
                Self::task_result_text(&result)
            ));
        }

        let payload: Value = serde_json::from_str(&Self::task_result_text(&result))
            .map_err(|err| format!("Invalid auto-request_review payload for {task_id}: {err}"))?;
        let review_id = payload
            .get("review_id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let panel_id = payload
            .get("panel_id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();

        Ok(Some(ReviewMaintenanceAction::AutoRequestedReview {
            task_id: task_id.to_string(),
            review_id,
            panel_id,
        }))
    }

    /// Create a new verification tool with default review configuration.
    pub fn new() -> Self {
        Self {
            config: ReviewConfig::default(),
            event_store: None,
            proof_recorder: super::proof::ReviewProofRecorder::empty(),
        }
    }

    /// Attach an event store for emitting review lifecycle events.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store.clone());
        self.proof_recorder = self.proof_recorder.with_event_store(store);
        self
    }

    /// Attach the proof projection used to durably record review evidence.
    pub fn with_proof_store(mut self, store: Arc<dyn ProofStore + Send + Sync>) -> Self {
        self.proof_recorder = self.proof_recorder.with_proof_store(store);
        self
    }

    /// Override the default review configuration (thresholds, panel settings).
    pub fn with_config(mut self, config: ReviewConfig) -> Self {
        self.config = config;
        self
    }

    pub async fn sweep_collecting_reviews(
        &self,
        requested_by: &str,
    ) -> Vec<ReviewMaintenanceAction> {
        let mut actions = Vec::new();

        if let Some(tasks_root) = Self::runtime_tasks_dir() {
            if let Ok(entries) = std::fs::read_dir(&tasks_root) {
                let mut task_ids = entries
                    .flatten()
                    .filter_map(|entry| {
                        let path = entry.path();
                        (path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                            .then(|| {
                                path.file_stem()
                                    .and_then(|stem| stem.to_str())
                                    .map(str::to_string)
                            })
                            .flatten()
                    })
                    .collect::<Vec<_>>();
                task_ids.sort();

                for task_id in &task_ids {
                    match self.recover_orphaned_review_gate_task(task_id).await {
                        Ok(Some(action)) => actions.push(action),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(
                                task_id,
                                error = %err,
                                "Failed to recover orphaned review gate during background sweep"
                            );
                        }
                    }
                }

                for task_id in &task_ids {
                    match self
                        .release_superseded_review_state(task_id, requested_by)
                        .await
                    {
                        Ok(Some(action)) => actions.push(action),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(
                                task_id,
                                error = %err,
                                "Failed to release stale review state during background sweep"
                            );
                        }
                    }
                }

                for task_id in &task_ids {
                    match self.release_dead_changes_requested_assignee(task_id).await {
                        Ok(Some(action)) => actions.push(action),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(
                                task_id,
                                error = %err,
                                "Failed to release dead changes_requested assignee during background sweep"
                            );
                        }
                    }
                }

                for task_id in &task_ids {
                    match self
                        .auto_request_review_ready_task(task_id, requested_by)
                        .await
                    {
                        Ok(Some(action)) => actions.push(action),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(
                                task_id,
                                error = %err,
                                "Failed to auto-request review for ready task during background sweep"
                            );
                        }
                    }
                }
            }
        }

        let Some(reviews_root) = reviews_dir() else {
            return actions;
        };
        let Ok(entries) = std::fs::read_dir(reviews_root) else {
            return actions;
        };

        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let task_id = entry.file_name().to_string_lossy().to_string();
            let _lock = match acquire_review_lock(&task_id).await {
                Ok(lock) => lock,
                Err(err) => {
                    tracing::warn!(task_id, error = %err, "Failed to lock review state during background sweep");
                    continue;
                }
            };

            let Some(mut state) = read_review_state(&task_id) else {
                continue;
            };
            if state.status != "collecting" {
                continue;
            }

            let review_id = state.current_review_id.clone();
            if self.check_timeout(&task_id, &mut state).await {
                let outcome = read_review_state(&task_id)
                    .map(|updated| updated.status)
                    .unwrap_or_else(|| state.status.clone());
                actions.push(ReviewMaintenanceAction::TimedOut {
                    task_id: task_id.clone(),
                    review_id,
                    outcome,
                });
                continue;
            }

            match self
                .reassign_dead_panel_members(&task_id, &mut state, requested_by)
                .await
            {
                Ok(Some(result)) => actions.push(ReviewMaintenanceAction::ReassignedPanel {
                    task_id: task_id.clone(),
                    review_id: result.review_id,
                    panel_id: result.panel_id,
                    replacements: result.replacements,
                }),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(task_id, error = %err, "Failed to auto-reassign dead panel members during background sweep");
                }
            }
        }

        actions
    }

    /// Emit a review lifecycle event if event store is available.
    pub(super) async fn emit_event(&self, kind: EventKind, aggregate_id: &str) {
        if let Some(ref store) = self.event_store {
            let event = Event {
                kind,
                timestamp: chrono::Utc::now(),
                aggregate_id: aggregate_id.to_string(),
            };
            if let Err(e) = store.append(event).await {
                tracing::warn!("Failed to emit review event: {e}");
            }
        }
    }

    pub(super) fn review_queue_priority(task: &Value) -> Priority {
        match task
            .get("priority")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("critical") => Priority::Critical,
            Some("high") => Priority::High,
            Some("low") => Priority::Low,
            Some("medium") | None | Some("") => Priority::Medium,
            Some(_) => Priority::Medium,
        }
    }

    pub(super) async fn emit_review_requested(
        &self,
        task: &Value,
        task_id: &str,
        review_id: &str,
    ) -> Result<(), String> {
        if let Some(ref store) = self.event_store {
            PriorityQueue::new(store.clone())
                .enqueue(review_id, task_id, Self::review_queue_priority(task))
                .await
                .map_err(|err| format!("Failed to durably enqueue review request: {err}"))?;
            return Ok(());
        }

        self.emit_event(
            EventKind::ReviewRequested {
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
            },
            review_id,
        )
        .await;
        Ok(())
    }

    pub(super) fn configured_panels(&self) -> Vec<ReviewPanelConfig> {
        if self.config.panels.is_empty() {
            vec![ReviewPanelConfig {
                id: IMPLICIT_PANEL_ID.to_string(),
                reviewers: self.config.default_reviewers.clone(),
            }]
        } else {
            self.config.panels.clone()
        }
    }

    fn configured_panels_for_acquisition(
        &self,
        active_leases: &[PanelLeaseState],
    ) -> Vec<ReviewPanelConfig> {
        let panels = self.configured_panels();
        if !self.share_after_submit_enabled() || panels.len() <= 1 {
            return panels;
        }

        let mut indexed: Vec<(usize, ReviewPanelConfig)> = panels.into_iter().enumerate().collect();
        indexed.sort_by(|(left_idx, left), (right_idx, right)| {
            let lease_counts_as_usage = |lease: &&PanelLeaseState| {
                !self.share_after_submit_enabled()
                    || !self.active_reserved_members(lease).is_empty()
            };
            let left_usage = active_leases
                .iter()
                .filter(|lease| lease.panel_id == left.id)
                .filter(|lease| lease_counts_as_usage(lease))
                .count();
            let right_usage = active_leases
                .iter()
                .filter(|lease| lease.panel_id == right.id)
                .filter(|lease| lease_counts_as_usage(lease))
                .count();

            left_usage
                .cmp(&right_usage)
                .then_with(|| {
                    let left_latest = active_leases
                        .iter()
                        .filter(|lease| lease.panel_id == left.id)
                        .filter(|lease| lease_counts_as_usage(lease))
                        .map(|lease| lease.updated_at.as_str())
                        .max();
                    let right_latest = active_leases
                        .iter()
                        .filter(|lease| lease.panel_id == right.id)
                        .filter(|lease| lease_counts_as_usage(lease))
                        .map(|lease| lease.updated_at.as_str())
                        .max();
                    left_latest.cmp(&right_latest)
                })
                .then_with(|| left_idx.cmp(right_idx))
        });

        indexed.into_iter().map(|(_, panel)| panel).collect()
    }

    pub(super) fn panel_mode_str(&self) -> &'static str {
        if self.config.panels.is_empty() {
            match self.config.panel_mode {
                ReviewPanelMode::FullCouncil => "full_council",
                ReviewPanelMode::FixedSize => "fixed_size",
            }
        } else {
            "configured_panel"
        }
    }

    pub(super) fn select_legacy_panel_members(&self) -> Vec<PanelLeaseMember> {
        self.select_legacy_panel_members_with_reserved(&HashSet::new())
    }

    pub(super) fn select_legacy_panel_members_with_reserved(
        &self,
        reserved_reviewers: &HashSet<String>,
    ) -> Vec<PanelLeaseMember> {
        let live_reviewers = find_agents_by_role_with_type("reviewer");
        let available_reviewers: Vec<AgentInfo> = live_reviewers
            .iter()
            .filter(|agent| !reserved_reviewers.contains(&agent.name))
            .map(|agent| AgentInfo {
                name: agent.name.clone(),
                agent_type: agent.agent_type.clone(),
            })
            .collect();
        let selected = match self.config.panel_mode {
            ReviewPanelMode::FullCouncil => {
                build_full_council_panel(&self.config.default_reviewers, &available_reviewers)
            }
            ReviewPanelMode::FixedSize => {
                let desired_size = if self.config.default_reviewers.is_empty() {
                    self.config.policy.min_approvals.max(2) as usize
                } else {
                    self.config
                        .default_reviewers
                        .len()
                        .max(self.config.policy.min_approvals as usize)
                };

                build_panel(
                    &self.config.default_reviewers,
                    &available_reviewers,
                    desired_size.min(available_reviewers.len()),
                )
            }
        };
        let live_by_name: HashMap<String, String> = live_reviewers
            .into_iter()
            .map(|agent| (agent.name, agent.agent_type))
            .collect();
        selected
            .into_iter()
            .map(|reviewer| PanelLeaseMember {
                slot_agent: live_by_name.get(&reviewer).cloned().unwrap_or_default(),
                reviewer,
            })
            .collect()
    }

    pub(super) fn resolve_configured_panel_members(
        &self,
        panel: &ReviewPanelConfig,
        live_reviewers: &[AgentInfo],
        reserved_reviewers: &HashSet<String>,
    ) -> Option<Vec<PanelLeaseMember>> {
        self.resolve_configured_panel_members_with_affinity(
            panel,
            live_reviewers,
            reserved_reviewers,
            &[],
        )
    }

    fn resolve_persisted_panel_seat_members(
        &self,
        panel: &ReviewPanelConfig,
        live_reviewers: &[AgentInfo],
        reserved_reviewers: &HashSet<String>,
    ) -> Option<Vec<PanelLeaseMember>> {
        let seat = read_panel_seat(&panel.id)?;
        if seat.panel_id != panel.id || seat.members.len() != panel.reviewers.len() {
            return None;
        }

        let live_by_name: HashMap<String, String> = live_reviewers
            .iter()
            .map(|agent| (agent.name.clone(), agent.agent_type.clone()))
            .collect();
        let mut seen_reviewers = HashSet::new();
        let mut members = Vec::with_capacity(seat.members.len());

        for (slot_agent, member) in panel.reviewers.iter().zip(seat.members) {
            if member.slot_agent != *slot_agent {
                return None;
            }
            if reserved_reviewers.contains(&member.reviewer)
                || !seen_reviewers.insert(member.reviewer.clone())
            {
                return None;
            }
            if live_by_name.get(&member.reviewer) != Some(slot_agent) {
                return None;
            }
            members.push(member);
        }

        Some(members)
    }

    pub(super) fn resolve_configured_panel_members_with_affinity(
        &self,
        panel: &ReviewPanelConfig,
        live_reviewers: &[AgentInfo],
        reserved_reviewers: &HashSet<String>,
        preferred_members: &[PanelLeaseMember],
    ) -> Option<Vec<PanelLeaseMember>> {
        if read_panel_seat(&panel.id).is_some() {
            return self.resolve_persisted_panel_seat_members(
                panel,
                live_reviewers,
                reserved_reviewers,
            );
        }

        let mut used_reviewers = reserved_reviewers.clone();
        let mut members = Vec::new();

        for slot_agent in &panel.reviewers {
            let preferred = preferred_members
                .iter()
                .find(|member| {
                    member.slot_agent == *slot_agent
                        && !used_reviewers.contains(&member.reviewer)
                        && live_reviewers.iter().any(|agent| {
                            agent.name == member.reviewer && agent.agent_type == *slot_agent
                        })
                })
                .map(|member| member.reviewer.clone());
            let reviewer = if let Some(reviewer) = preferred {
                reviewer
            } else {
                live_reviewers
                    .iter()
                    .filter(|agent| agent.agent_type == *slot_agent)
                    .map(|agent| agent.name.clone())
                    .filter(|name| !used_reviewers.contains(name))
                    .min()?
            };
            used_reviewers.insert(reviewer.clone());
            members.push(PanelLeaseMember {
                slot_agent: slot_agent.clone(),
                reviewer,
            });
        }

        Some(members)
    }

    pub(super) fn refresh_reused_panel_members(
        &self,
        lease: &PanelLeaseState,
        active_leases: &[PanelLeaseState],
    ) -> Result<Vec<PanelLeaseMember>, String> {
        if self.config.panels.is_empty() {
            let refreshed = self.select_legacy_panel_members();
            if refreshed.is_empty() {
                return Err(format!(
                    "Task {} owns review panel '{}' but no reviewers are currently available for rereview.",
                    lease.task_id, lease.panel_id
                ));
            }
            return Ok(refreshed);
        }

        let Some(panel) = self
            .configured_panels()
            .into_iter()
            .find(|panel| panel.id == lease.panel_id)
        else {
            return Err(format!(
                "Task {} owns review panel '{}' but that panel is no longer configured. \
                 A supervisor must restore the panel config or run verification action=release_panel task_id={} reason=\"...\".",
                lease.task_id, lease.panel_id, lease.task_id
            ));
        };

        let live_reviewers = find_agents_by_role_with_type("reviewer");
        let reserved_reviewers: HashSet<String> = active_leases
            .iter()
            .filter(|other| other.task_id != lease.task_id)
            .flat_map(|other| self.active_reserved_members(other).into_iter())
            .map(|member| member.reviewer)
            .collect();

        self.resolve_configured_panel_members_with_affinity(
            &panel,
            &live_reviewers,
            &reserved_reviewers,
            &lease.members,
        )
        .ok_or_else(|| {
            format!(
                "Task {} owns review panel '{}' but Brehon could not seat live reviewers for every slot on rereview. \
                 A supervisor must bring the required reviewer sessions online, add panel capacity, or release/reseat the panel.",
                lease.task_id, lease.panel_id
            )
        })
    }

    pub(super) fn lease_busy_message(&self, requested_task_id: &str) -> String {
        let mut active_leases = read_all_panel_leases();
        active_leases.sort_by(|a, b| a.panel_id.cmp(&b.panel_id));

        if active_leases.is_empty() {
            return format!(
                "No idle review panels are currently available for task {requested_task_id}. \
                 Configure review.panels or bring reviewer sessions online."
            );
        }

        let details = active_leases
            .iter()
            .map(|lease| format!("{} -> {}", lease.panel_id, lease.task_id))
            .collect::<Vec<_>>()
            .join(", ");

        format!(
            "No idle review panels are currently available for task {requested_task_id}. \
             Active panel leases: {details}. Wait for a panel to be released or add more review panels."
        )
    }

    fn configured_panel_shortfall_details(
        &self,
        live_reviewers: &[AgentInfo],
        reserved_reviewers: &HashSet<String>,
    ) -> Vec<String> {
        self.configured_panels()
            .into_iter()
            .filter_map(|panel| {
                if let Some(seat) = read_panel_seat(&panel.id) {
                    let live_by_name: HashMap<String, String> = live_reviewers
                        .iter()
                        .map(|agent| (agent.name.clone(), agent.agent_type.clone()))
                        .collect();
                    let mut unavailable = Vec::new();
                    if seat.members.len() != panel.reviewers.len() {
                        unavailable.push(format!(
                            "seat count {} != configured slots {}",
                            seat.members.len(),
                            panel.reviewers.len()
                        ));
                    }
                    for (idx, slot_agent) in panel.reviewers.iter().enumerate() {
                        let Some(member) = seat.members.get(idx) else {
                            unavailable.push(format!("{slot_agent} missing planned reviewer"));
                            continue;
                        };
                        if member.slot_agent != *slot_agent {
                            unavailable.push(format!(
                                "{} planned as {}",
                                member.reviewer, member.slot_agent
                            ));
                        } else if reserved_reviewers.contains(&member.reviewer) {
                            unavailable.push(format!("{} reserved", member.reviewer));
                        } else if live_by_name.get(&member.reviewer) != Some(slot_agent) {
                            unavailable
                                .push(format!("{} not live as {}", member.reviewer, slot_agent));
                        }
                    }

                    return (!unavailable.is_empty())
                        .then(|| format!("{} unavailable [{}]", panel.id, unavailable.join(", ")));
                }

                let mut used_reviewers = reserved_reviewers.clone();
                let mut missing_slots = Vec::new();

                for slot_agent in &panel.reviewers {
                    let next = live_reviewers
                        .iter()
                        .filter(|agent| agent.agent_type == *slot_agent)
                        .map(|agent| agent.name.clone())
                        .filter(|name| !used_reviewers.contains(name))
                        .min();

                    if let Some(reviewer) = next {
                        used_reviewers.insert(reviewer);
                    } else {
                        missing_slots.push(slot_agent.clone());
                    }
                }

                (!missing_slots.is_empty())
                    .then(|| format!("{} missing [{}]", panel.id, missing_slots.join(", ")))
            })
            .collect()
    }

    fn live_reviewer_lane_summary(&self, live_reviewers: &[AgentInfo]) -> String {
        let mut counts = HashMap::<String, usize>::new();
        for reviewer in live_reviewers {
            *counts.entry(reviewer.agent_type.clone()).or_insert(0) += 1;
        }

        let mut entries: Vec<(String, usize)> = counts.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        if entries.is_empty() {
            return "(none)".to_string();
        }

        entries
            .into_iter()
            .map(|(lane, count)| format!("{lane} x{count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn acquire_or_reuse_panel_lease(
        &self,
        task_id: &str,
        review_id: &str,
        round: u32,
        timestamp: &str,
    ) -> Result<PanelLeaseState, String> {
        let active_leases = read_all_panel_leases();

        if let Some(mut lease) = active_leases
            .iter()
            .find(|lease| lease.task_id == task_id)
            .cloned()
        {
            lease.members = self.refresh_reused_panel_members(&lease, &active_leases)?;
            lease.review_id = review_id.to_string();
            lease.round = round;
            lease.updated_at = timestamp.to_string();
            write_panel_lease(&lease).map_err(|err| {
                format!("Failed to persist panel lease for task {task_id}: {err}")
            })?;
            return Ok(lease);
        }

        if self.config.panels.is_empty() {
            if !self.share_after_submit_enabled()
                && active_leases
                    .iter()
                    .any(|lease| lease.panel_id == IMPLICIT_PANEL_ID)
            {
                return Err(self.lease_busy_message(task_id));
            }

            let reserved_reviewers =
                self.reserved_reviewers_for_active_leases(&active_leases, Some(task_id));
            let members = self.select_legacy_panel_members_with_reserved(&reserved_reviewers);
            if members.is_empty() {
                return Err(
                    "No reviewers available. Ensure reviewer agents are registered.".to_string(),
                );
            }

            let lease = PanelLeaseState {
                panel_id: IMPLICIT_PANEL_ID.to_string(),
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
                round,
                members,
                leased_at: timestamp.to_string(),
                updated_at: timestamp.to_string(),
            };
            write_panel_lease(&lease).map_err(|err| {
                format!("Failed to persist panel lease for task {task_id}: {err}")
            })?;
            return Ok(lease);
        }

        let live_reviewers = find_agents_by_role_with_type("reviewer");
        // In ShareAfterSubmit mode we don't block at the panel level — a
        // partially-released panel can still be seated for a different task
        // as long as the released slots have free reviewers to fill them.
        // `reserved_reviewers` (built from `active_reserved_members`)
        // already prevents double-booking still-reserved members, and
        // `resolve_configured_panel_members` fails closed if no seating
        // exists. The legacy lease modes keep the strict per-panel block.
        let leased_panels: HashSet<String> = if self.share_after_submit_enabled() {
            HashSet::new()
        } else {
            active_leases
                .iter()
                .map(|lease| lease.panel_id.clone())
                .collect()
        };
        let reserved_reviewers =
            self.reserved_reviewers_for_active_leases(&active_leases, Some(task_id));

        for panel in self.configured_panels_for_acquisition(&active_leases) {
            if leased_panels.contains(&panel.id) {
                continue;
            }
            let Some(members) =
                self.resolve_configured_panel_members(&panel, &live_reviewers, &reserved_reviewers)
            else {
                continue;
            };

            let lease = PanelLeaseState {
                panel_id: panel.id,
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
                round,
                members,
                leased_at: timestamp.to_string(),
                updated_at: timestamp.to_string(),
            };
            write_panel_lease(&lease).map_err(|err| {
                format!("Failed to persist panel lease for task {task_id}: {err}")
            })?;
            return Ok(lease);
        }

        if active_leases.is_empty() {
            let shortfalls =
                self.configured_panel_shortfall_details(&live_reviewers, &reserved_reviewers);
            if !shortfalls.is_empty() {
                return Err(format!(
                    "No configured review panel could be seated for task {task_id}. \
                     Panel seat shortfalls: {}. Live reviewer sessions by lane: {}.",
                    shortfalls.join("; "),
                    self.live_reviewer_lane_summary(&live_reviewers),
                ));
            }
        }

        Err(self.lease_busy_message(task_id))
    }

    pub(super) fn reseated_panel_members(
        &self,
        task_id: &str,
        state: &ReviewState,
        requested_panel_id: Option<&str>,
    ) -> Result<(String, Vec<PanelLeaseMember>), String> {
        if state.panel.is_empty() {
            return Err(format!(
                "Review {} for task {task_id} has no panel members to reseat.",
                state.current_review_id
            ));
        }

        let live_reviewers = find_agents_by_role_with_type("reviewer");
        let live_by_name: HashMap<String, String> = live_reviewers
            .iter()
            .map(|agent| (agent.name.clone(), agent.agent_type.clone()))
            .collect();
        let active_leases = read_all_panel_leases();

        if self.config.panels.is_empty() {
            if !self.share_after_submit_enabled()
                && active_leases
                    .iter()
                    .any(|lease| lease.panel_id == IMPLICIT_PANEL_ID && lease.task_id != task_id)
            {
                return Err(self.lease_busy_message(task_id));
            }

            let panel_id = requested_panel_id
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(IMPLICIT_PANEL_ID)
                .to_string();
            let members = state
                .panel
                .iter()
                .map(|reviewer| PanelLeaseMember {
                    slot_agent: live_by_name.get(reviewer).cloned().unwrap_or_default(),
                    reviewer: reviewer.clone(),
                })
                .collect();
            return Ok((panel_id, members));
        }

        let panel_id = requested_panel_id
            .filter(|value| !value.trim().is_empty())
            .or_else(|| (!state.panel_id.trim().is_empty()).then_some(state.panel_id.as_str()))
            .ok_or_else(|| {
                format!(
                    "Review {} for task {task_id} has no panel_id. Pass panel_id=<configured-panel> to reseat it.",
                    state.current_review_id
                )
            })?;

        let configured_panel = self
            .configured_panels()
            .into_iter()
            .find(|panel| panel.id == panel_id)
            .ok_or_else(|| {
                let available = self
                    .configured_panels()
                    .into_iter()
                    .map(|panel| panel.id)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Cannot reseat review {} for task {task_id}: panel '{}' is not configured. Available panels: {}",
                    state.current_review_id,
                    panel_id,
                    if available.is_empty() { "(none)".to_string() } else { available }
                )
            })?;

        if !self.share_after_submit_enabled()
            && active_leases
                .iter()
                .any(|lease| lease.panel_id == panel_id && lease.task_id != task_id)
        {
            return Err(self.lease_busy_message(task_id));
        }

        let reserved_reviewers =
            self.reserved_reviewers_for_active_leases(&active_leases, Some(task_id));
        if read_panel_seat(&configured_panel.id).is_some() {
            let members = self
                .resolve_persisted_panel_seat_members(
                    &configured_panel,
                    &live_reviewers,
                    &reserved_reviewers,
                )
                .ok_or_else(|| {
                    format!(
                        "Cannot reseat review {} for task {task_id}: configured panel '{}' has no complete live physical seat map for this run.",
                        state.current_review_id, configured_panel.id
                    )
                })?;
            return Ok((panel_id.to_string(), members));
        }

        let overlapping_reviewers: Vec<String> = state
            .panel
            .iter()
            .filter(|reviewer| reserved_reviewers.contains(*reviewer))
            .cloned()
            .collect();
        if !overlapping_reviewers.is_empty() {
            return Err(format!(
                "Cannot reseat review {} for task {task_id}: reviewer(s) already reserved by another leased panel: {}",
                state.current_review_id,
                overlapping_reviewers.join(", ")
            ));
        }

        let members = state
            .panel
            .iter()
            .enumerate()
            .map(|(idx, reviewer)| PanelLeaseMember {
                slot_agent: live_by_name
                    .get(reviewer)
                    .cloned()
                    .or_else(|| configured_panel.reviewers.get(idx).cloned())
                    .unwrap_or_default(),
                reviewer: reviewer.clone(),
            })
            .collect();

        Ok((panel_id.to_string(), members))
    }

    pub(super) fn replacement_candidates_for_reviewer(
        &self,
        lease: Option<&PanelLeaseState>,
        state: &ReviewState,
        dead_reviewer: &str,
    ) -> Vec<String> {
        let live_reviewers = find_agents_by_role_with_type("reviewer");
        let current_reviewers: HashSet<String> = state.panel.iter().cloned().collect();

        let mut candidates: Vec<String> = if let Some(slot_agent) = lease
            .and_then(|lease| {
                lease
                    .members
                    .iter()
                    .find(|member| member.reviewer == dead_reviewer)
                    .map(|member| member.slot_agent.clone())
            })
            .filter(|slot_agent| !slot_agent.is_empty())
        {
            let same_slot: Vec<String> = live_reviewers
                .iter()
                .filter(|agent| agent.agent_type == slot_agent)
                .map(|agent| agent.name.clone())
                .filter(|name| !current_reviewers.contains(name))
                .collect();
            if same_slot.is_empty() {
                live_reviewers
                    .iter()
                    .map(|agent| agent.name.clone())
                    .filter(|name| !current_reviewers.contains(name))
                    .collect()
            } else {
                same_slot
            }
        } else {
            let mut legacy_candidates = self.select_legacy_panel_members();
            legacy_candidates.retain(|member| !current_reviewers.contains(&member.reviewer));
            legacy_candidates
                .into_iter()
                .map(|member| member.reviewer)
                .collect()
        };

        candidates.sort();
        candidates.dedup();
        candidates
    }

    pub(super) fn replacement_candidates(&self, state: &ReviewState) -> Vec<String> {
        let lease = find_panel_lease_by_task(&state.task_id);
        let mut candidates = Vec::new();
        for dead in self.find_dead_panel_members(state) {
            candidates.extend(self.replacement_candidates_for_reviewer(
                lease.as_ref(),
                state,
                &dead,
            ));
        }
        candidates.sort();
        candidates.dedup();
        candidates
    }

    pub(super) fn pending_panel_reviewers(state: &ReviewState) -> Vec<String> {
        state
            .panel
            .iter()
            .filter(|reviewer| !state.submissions_received.contains(*reviewer))
            .cloned()
            .collect()
    }

    pub(super) fn notify_pending_panel_reviewers(
        &self,
        state: &ReviewState,
        from: &str,
        message: &str,
    ) -> Vec<String> {
        Self::pending_panel_reviewers(state)
            .into_iter()
            .filter(|reviewer| try_deliver_message(reviewer, from, message).queued)
            .collect()
    }

    pub(super) fn ignored_submit_review_result(
        &self,
        review_id: &str,
        task_id: &str,
        reason: RejectionReason,
        message: String,
        state: Option<&ReviewState>,
        task_status: Option<&str>,
    ) -> ToolResult {
        let mut result = serde_json::json!({
            "status": "ok",
            "ignored": true,
            "reason": reason.as_str(),
            "review_id": review_id,
            "task_id": task_id,
            "message": message,
        });

        if let Some(state) = state {
            result["active_review_id"] = serde_json::json!(state.current_review_id);
            result["review_status"] = serde_json::json!(state.status);
            result["round"] = serde_json::json!(state.current_round);
        }

        if let Some(task_status) = task_status {
            result["task_status"] = serde_json::json!(task_status);
        }

        text_result(
            serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| "{\"status\":\"ok\",\"ignored\":true}".to_string()),
        )
    }

    pub(super) fn submit_review_rejection_result(
        &self,
        review_id: &str,
        reason: RejectionReason,
        message: String,
    ) -> ToolResult {
        let result = serde_json::json!({
            "status": "error",
            "reason": reason.as_str(),
            "review_id": review_id,
            "message": message,
        });

        error_result(serde_json::to_string_pretty(&result).unwrap_or_else(|_| {
            format!(
                "{{\"status\":\"error\",\"reason\":\"{}\"}}",
                reason.as_str()
            )
        }))
    }

    pub(super) async fn reassign_dead_panel_members(
        &self,
        task_id: &str,
        state: &mut ReviewState,
        requested_by: &str,
    ) -> Result<Option<PanelReassignmentResult>, String> {
        let mut lease = find_panel_lease_by_task(task_id);
        let live_by_name: HashMap<String, String> = find_agents_by_role_with_type("reviewer")
            .into_iter()
            .map(|agent| (agent.name, agent.agent_type))
            .collect();
        let dead = self.find_dead_panel_members(state);
        if dead.is_empty() {
            return Ok(None);
        }

        let mut replacements: Vec<PanelReviewerReplacement> = Vec::new();
        for dead_reviewer in &dead {
            let candidates =
                self.replacement_candidates_for_reviewer(lease.as_ref(), state, dead_reviewer);
            let Some(replacement) = candidates.first().cloned() else {
                return Err(format!(
                    "Cannot preserve the frozen review council for task {task_id}: panel '{}' has dead reviewer '{}' with no eligible replacement. \
                     Bring the reviewer back, add another '{}' reviewer, wait for timeout escalation, or use verification action=release_panel task_id={task_id} reason=\"...\".",
                    state.panel_id,
                    dead_reviewer,
                    lease
                        .as_ref()
                        .and_then(|current| {
                            current
                                .members
                                .iter()
                                .find(|member| member.reviewer == *dead_reviewer)
                                .map(|member| member.slot_agent.as_str())
                        })
                        .unwrap_or("reviewer")
                ));
            };
            replacements.push(PanelReviewerReplacement {
                removed: dead_reviewer.clone(),
                replaced_with: replacement.clone(),
            });
            if let Some(pos) = state.panel.iter().position(|r| r == dead_reviewer) {
                state.panel[pos] = replacement.clone();
            }
            if let Some(current_lease) = lease.as_mut() {
                if let Some(member) = current_lease
                    .members
                    .iter_mut()
                    .find(|member| member.reviewer == *dead_reviewer)
                {
                    member.reviewer = replacement;
                    if let Some(replacement_agent_type) = live_by_name.get(&member.reviewer) {
                        member.slot_agent = replacement_agent_type.clone();
                    }
                }
            }
        }

        state.updated_at = chrono::Utc::now().to_rfc3339();
        write_review_state(task_id, state).map_err(|err| {
            format!("Failed to persist reassigned panel for task {task_id}: {err}")
        })?;
        if let Some(current_lease) = lease.as_mut() {
            current_lease.updated_at = chrono::Utc::now().to_rfc3339();
            current_lease.review_id = state.current_review_id.clone();
            current_lease.round = state.current_round;
            write_panel_lease(current_lease).map_err(|err| {
                format!("Failed to persist reassigned panel lease for task {task_id}: {err}")
            })?;
        }

        let mut request = super::state::read_round_request(task_id, state.current_round)
            .ok_or_else(|| {
                format!(
                    "Review request metadata is missing for task {task_id} round {}. \
                 Cannot reassign panel without request.json.",
                    state.current_round
                )
            })?;
        let title = request.title.clone();
        let description = request.description.clone();
        let commit = request.commit.clone();
        let context = request.context.clone();
        let base_commit_owned = request.base_commit.trim().to_string();
        let base_commit = (!base_commit_owned.is_empty()).then_some(base_commit_owned.as_str());
        let task_for_prompt = read_task(task_id);
        let worker_branch = task_for_prompt
            .as_ref()
            .and_then(|task| task.get("branch"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let merge_target = read_task_merge_target(task_id)
            .or_else(detect_default_branch)
            .unwrap_or_else(|| "main".to_string());
        let reassignment_context = if context.trim().is_empty() {
            "panel reassignment: previous reviewer left before submitting. Review this task as a fresh panel member.".to_string()
        } else {
            format!(
                "panel reassignment: previous reviewer left before submitting. Review this task as a fresh panel member.\n\n{context}"
            )
        };

        let new_reviewers: Vec<String> = state
            .panel
            .iter()
            .filter(|r| !state.submissions_received.contains(r))
            .cloned()
            .collect();

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
        let research_context = if project_config
            .as_ref()
            .is_none_or(|config| config.research.attach.on_review_request)
        {
            task_for_prompt
                .as_ref()
                .map(|task| {
                    crate::tools::research::render_task_research_handoff(
                        task,
                        project_config.as_ref(),
                    )
                })
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut rendered_review_prompts = Vec::new();
        for reviewer in &new_reviewers {
            let review_prompt = build_review_request_prompt(&ReviewRequestPromptInput {
                review_id: &state.current_review_id,
                task_id,
                title: &title,
                description: &description,
                context: &reassignment_context,
                panel_id: &state.panel_id,
                round: state.current_round,
                reviewer,
                commit: &commit,
                base_commit,
                worker_branch,
                merge_target: Some(&merge_target),
                review_fingerprint: Some(&request.review_fingerprint),
                proof_summary: proof_summary.as_ref(),
                research_context: Some(&research_context),
            });
            request
                .reviewer_prompts
                .insert(reviewer.clone(), review_prompt.clone());
            rendered_review_prompts.push((reviewer.clone(), review_prompt));
        }
        write_round_request(task_id, state.current_round, &request).map_err(|err| {
            format!(
                "Failed to persist reassigned review request metadata for task {task_id}: {err}"
            )
        })?;

        for (reviewer, review_prompt) in rendered_review_prompts {
            let delivery = try_deliver_message(&reviewer, requested_by, &review_prompt);
            state.reviewer_assignments.insert(
                reviewer.clone(),
                AssignmentPropagation::new(
                    &reviewer,
                    "review",
                    (!delivery.prompt_id.trim().is_empty()).then(|| delivery.prompt_id.clone()),
                    Some(delivery.method.clone()),
                ),
            );
        }
        write_review_state(task_id, state).map_err(|err| {
            format!(
                "Failed to persist reassigned reviewer assignment propagation for task {task_id}: {err}"
            )
        })?;

        Ok(Some(PanelReassignmentResult {
            review_id: state.current_review_id.clone(),
            panel_id: state.panel_id.clone(),
            panel: state.panel.clone(),
            replacements,
            prompts_sent_to: new_reviewers,
            submissions_already_received: state.submissions_received.clone(),
        }))
    }

    // ── Timeout & health checks ───────────────────────────────────────────

    /// Check if a review has timed out based on `timeout_minutes` config.
    /// Returns `true` if the review state was updated to "timed_out" / escalated.
    pub(super) async fn check_timeout(&self, task_id: &str, state: &mut ReviewState) -> bool {
        if state.status != "collecting" {
            return false;
        }
        let timeout_mins = self.config.timeout_minutes as i64;
        if timeout_mins == 0 {
            return false; // timeout disabled
        }
        let created = match chrono::DateTime::parse_from_rfc3339(&state.created_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => return false,
        };
        let elapsed = chrono::Utc::now() - created;
        if elapsed.num_minutes() >= timeout_mins {
            let timeout_notice = format!(
                "Review {} for task {task_id} timed out and is no longer active. Stop reviewing this round. If a fresh round is needed, you will receive a new review id. Any late submission will be ignored.",
                state.current_review_id
            );
            // Timeout is an operator stop. Do not evaluate partial panels: a
            // missing reviewer cannot be converted into approval or changes
            // requested by policy shortcut.
            let submissions = read_round_submissions(task_id, state.current_round);
            if submissions.is_empty() {
                let total_rounds_exhausted = total_review_rounds_exhausted(state);
                let status_to_set = if total_rounds_exhausted {
                    "blocked"
                } else {
                    "changes_requested"
                };
                if let Err(err) = update_task_status_atomic(task_id, status_to_set).await {
                    tracing::warn!(task_id, error = %err, "Failed to update task status after timeout");
                    return false;
                }
                let timeout_feedback = serde_json::json!({
                    "review_id": state.current_review_id,
                    "round": state.current_round,
                    "outcome": "escalated",
                    "panel_id": state.panel_id,
                    "panel": state.panel,
                    "threshold_result": "timed_out",
                    "threshold_reason": format!(
                        "No reviewers submitted within {} minutes. Supervisor must request a fresh round, reset review rounds, reseat/reassign reviewers, or mark the task changes_requested/rejected. Approval override is disabled.",
                        timeout_mins
                    ),
                    "blocking": [],
                    "suggestions": [],
                    "nitpicks": [],
                    "dissent": [],
                    "evaluated_at": chrono::Utc::now().to_rfc3339(),
                });
                if let Err(err) = set_task_review_feedback(task_id, Some(timeout_feedback)).await {
                    tracing::warn!(task_id, error = %err, "Failed to persist timeout review feedback");
                }
                state.status = "escalated".to_string();
                state.updated_at = chrono::Utc::now().to_rfc3339();
                if let Err(err) = write_review_state(task_id, state) {
                    tracing::warn!(task_id, error = %err, "Failed to persist timed-out review state");
                }
                let msg = format!(
                    "Review timeout for task {task_id} (round {}). \
                     No reviewers submitted within {} minutes. \
                     {}",
                    state.current_round,
                    timeout_mins,
                    if total_rounds_exhausted {
                        format!(
                            "Total review round limit ({}) is exhausted. The task was blocked for supervisor/manual intervention.",
                            total_review_round_limit(state.max_rounds)
                        )
                    } else if current_review_cycle_round(state) >= state.max_rounds as u32 {
                        format!(
                            "Current review cycle is exhausted. Reset rounds with: verification action=reset_rounds task_id={task_id} reason=\"...\" \
                             or mark changes requested/rejected with: verification action=override verdict=needs_revision ..."
                        )
                    } else {
                        format!(
                            "Start a fresh round with: verification action=request_review task_id={task_id}. Approval override is disabled."
                        )
                    }
                );
                self.notify_pending_panel_reviewers(state, "review-coordinator", &timeout_notice);
                notify_review_stakeholders(
                    task_id,
                    state.current_round,
                    "review-coordinator",
                    &msg,
                );
                return true;
            }
            if submissions.len() < state.panel.len() {
                let total_rounds_exhausted = total_review_rounds_exhausted(state);
                if total_rounds_exhausted {
                    if let Err(err) = update_task_status_atomic(task_id, "blocked").await {
                        tracing::warn!(task_id, error = %err, "Failed to update task status after timeout");
                        return false;
                    }
                }
                let submitted_reviewers: Vec<String> =
                    submissions.iter().map(|sub| sub.reviewer.clone()).collect();
                let pending_reviewers: Vec<String> = state
                    .panel
                    .iter()
                    .filter(|reviewer| !submitted_reviewers.contains(*reviewer))
                    .cloned()
                    .collect();
                let partial_submissions: Vec<serde_json::Value> = submissions
                    .iter()
                    .map(|submission| {
                        serde_json::json!({
                            "reviewer": submission.reviewer,
                            "score": submission.score,
                            "verdict": submission.verdict,
                            "summary": submission.summary,
                        })
                    })
                    .collect();
                let timeout_feedback = serde_json::json!({
                    "review_id": state.current_review_id,
                    "round": state.current_round,
                    "outcome": "escalated",
                    "panel_id": state.panel_id,
                    "panel": state.panel,
                    "submitted_reviewers": submitted_reviewers,
                    "pending_reviewers": pending_reviewers,
                    "partial_submissions": partial_submissions,
                    "threshold_result": "incomplete_quorum",
                    "threshold_reason": format!(
                        "Review timed out after {} minutes with {}/{} panel submissions. Incomplete review quorum cannot approve work or request changes. Supervisor must request a fresh round, reset review rounds, or reseat/reassign missing reviewers. Approval override is disabled.",
                        timeout_mins,
                        submissions.len(),
                        state.panel.len()
                    ),
                    "blocking": [],
                    "suggestions": [],
                    "nitpicks": [],
                    "dissent": [],
                    "evaluated_at": chrono::Utc::now().to_rfc3339(),
                });
                if let Err(err) = set_task_review_feedback(task_id, Some(timeout_feedback)).await {
                    tracing::warn!(task_id, error = %err, "Failed to persist timeout review feedback");
                }
                state.status = "escalated".to_string();
                state.updated_at = chrono::Utc::now().to_rfc3339();
                if let Err(err) = write_review_state(task_id, state) {
                    tracing::warn!(task_id, error = %err, "Failed to persist timed-out review state");
                }
                let msg = format!(
                    "Review timeout for task {task_id} (round {}) with {}/{} submissions. \
                     Incomplete quorum was not evaluated, and no terminal review outcome was produced. \
                     {}",
                    state.current_round,
                    submissions.len(),
                    state.panel.len(),
                    if total_rounds_exhausted {
                        format!(
                            "Total review round limit ({}) is exhausted. The task was blocked for supervisor/manual intervention.",
                            total_review_round_limit(state.max_rounds)
                        )
                    } else if current_review_cycle_round(state) >= state.max_rounds as u32 {
                        format!(
                            "Current review cycle is exhausted. Reset rounds with: verification action=reset_rounds task_id={task_id} reason=\"...\" \
                             or mark changes requested/rejected with: verification action=override verdict=needs_revision ..."
                        )
                    } else {
                        format!(
                            "Start a fresh round with: verification action=request_review task_id={task_id}. Approval override is disabled."
                        )
                    }
                );
                self.notify_pending_panel_reviewers(state, "review-coordinator", &timeout_notice);
                notify_review_stakeholders(
                    task_id,
                    state.current_round,
                    "review-coordinator",
                    &msg,
                );
                return true;
            }

            // Defensive repair for a collecting state that already has every
            // reviewer file on disk. Normal submit_review normally handles this.
            let report =
                self.evaluate_round(task_id, &state.current_review_id, state, &submissions);
            let status_to_set = task_status_for_report(state, &report);
            if let Err(err) = update_task_status_atomic(task_id, status_to_set).await {
                tracing::warn!(task_id, error = %err, "Failed to update task status after timeout evaluation");
                return false;
            }
            let feedback = Some(build_task_review_feedback(state, &report));
            if let Err(err) = set_task_review_feedback(task_id, feedback).await {
                tracing::warn!(task_id, error = %err, "Failed to persist timeout review feedback");
            }
            if report.outcome == "approved" {
                let followups = build_task_review_followups(&report);
                if let Err(err) = append_task_review_followups(task_id, &followups).await {
                    tracing::warn!(task_id, error = %err, "Failed to persist timeout review followups");
                }
            }
            if let Err(err) = write_consolidated(task_id, state.current_round, &report) {
                tracing::warn!(task_id, error = %err, "Failed to persist timeout consolidated report");
            }
            state.status = report.outcome.clone();
            state.updated_at = chrono::Utc::now().to_rfc3339();
            if let Err(err) = write_review_state(task_id, state) {
                tracing::warn!(task_id, error = %err, "Failed to persist timeout review state");
            }
            let notification = format!(
                "Review timeout for task {task_id} — evaluated with {}/{} submissions.\n{}",
                submissions.len(),
                state.panel.len(),
                self.format_consolidated_report(task_id, &report)
            );
            self.notify_pending_panel_reviewers(state, "review-coordinator", &timeout_notice);
            notify_review_stakeholders(
                task_id,
                state.current_round,
                "review-coordinator",
                &notification,
            );
            return true;
        }
        false
    }

    /// Check if panel members are still live. Returns list of dead reviewers.
    pub(super) fn check_panel_health(&self, state: &ReviewState) -> Vec<String> {
        if state.status != "collecting" {
            return Vec::new();
        }
        let live_reviewers = find_agents_by_role("reviewer");
        state
            .panel
            .iter()
            .filter(|r| !live_reviewers.contains(r) && !state.submissions_received.contains(r))
            .cloned()
            .collect()
    }

    /// Identify panel members that are no longer live (no active session).
    pub(super) fn find_dead_panel_members(&self, state: &ReviewState) -> Vec<String> {
        let live_reviewers = find_agents_by_role("reviewer");
        state
            .panel
            .iter()
            .filter(|r| !live_reviewers.contains(r) && !state.submissions_received.contains(r))
            .cloned()
            .collect()
    }

    /// Lightweight stale detection: compare stored commit hash with current HEAD.
    pub(super) fn check_stale(&self, task_id: &str, state: &ReviewState) -> Option<String> {
        if !self.config.stale_detection.enabled {
            return None;
        }
        let round = state.current_round;
        let dir = round_dir(task_id, round)?;
        let request_path = dir.join("request.json");
        let content = std::fs::read_to_string(request_path).ok()?;
        let request: ReviewRequestFile = serde_json::from_str(&content).ok()?;
        if request.commit.is_empty() {
            return None;
        }
        let current_head = current_git_head_short()?;
        if !request.commit.starts_with(&current_head) && !current_head.starts_with(&request.commit)
        {
            Some(format!(
                "Review may be stale: review was based on commit {} but HEAD is now {}",
                request.commit, current_head
            ))
        } else {
            None
        }
    }

    // ── Evaluation pipeline ──────────────────────────────────────────────

    pub(super) fn evaluate_round(
        &self,
        task_id: &str,
        review_id: &str,
        state: &ReviewState,
        submissions: &[StoredSubmission],
    ) -> ConsolidatedReport {
        // Step 1: Score collection using brehon-review
        let mut collector = ScoreCollector::new();
        let mut scores_map = serde_json::Map::new();
        let mut ignored_negative_reviews = Vec::new();

        for sub in submissions {
            let score = ReviewScore::new(sub.score);
            let verdict = parse_verdict(&sub.verdict);
            let mut score_entry = serde_json::json!({
                    "score": sub.score,
                    "verdict": sub.verdict
            });

            if let Some(reason) = unsupported_negative_review_reason(
                &self.config.policy,
                sub.score,
                &sub.verdict,
                &sub.findings,
            ) {
                score_entry["ignored_for_threshold"] = Value::Bool(true);
                score_entry["ignored_reason"] = Value::String(reason.clone());
                ignored_negative_reviews.push(format!("{}: {reason}", sub.reviewer));
            } else {
                collector.add(sub.reviewer.clone(), score, verdict);
            }

            scores_map.insert(sub.reviewer.clone(), score_entry);
        }

        // Step 2: Threshold evaluation using brehon-review
        let evaluator = ThresholdEvaluator::new(self.config.policy.clone());
        let threshold = evaluator.evaluate(&collector);

        let (outcome, threshold_reason) = match threshold {
            brehon_review::ThresholdResult::Approved => {
                let reason = if ignored_negative_reviews.is_empty() {
                    "All thresholds met".to_string()
                } else {
                    format!(
                        "All thresholds met after ignoring {} unsupported negative review(s) with no blocking findings",
                        ignored_negative_reviews.len()
                    )
                };
                ("approved".to_string(), reason)
            }
            brehon_review::ThresholdResult::ChangesRequested => {
                let reason = if collector.has_blocking_findings() {
                    format!(
                        "Score {} <= blocking_score {}",
                        collector.min_score().map_or(0, |s| s.as_u8()),
                        self.config.policy.blocking_score
                    )
                } else if let Some(avg) = collector.average_score() {
                    if avg < self.config.policy.min_average_score as f64 {
                        format!(
                            "Average {:.1} < min_average_score {}",
                            avg, self.config.policy.min_average_score
                        )
                    } else {
                        format!(
                            "Min score {} < min_individual_score {}",
                            collector.min_score().map_or(0, |s| s.as_u8()),
                            self.config.policy.min_individual_score
                        )
                    }
                } else {
                    "Changes requested by reviewer".to_string()
                };

                // Hard-stop total review churn across reset cycles. A reset
                // moves `cycle_start_round`, so the per-cycle cap alone cannot
                // catch livelock like round 44/cycle round 1.
                if total_review_rounds_exhausted(state) {
                    let total_limit = total_review_round_limit(state.max_rounds);
                    (
                        "escalated".to_string(),
                        format!(
                            "{reason}. Total review round limit ({total_limit}) exhausted across reset cycles — blocking task for supervisor/manual intervention."
                        ),
                    )
                } else if current_review_cycle_round(state) >= state.max_rounds as u32 {
                    (
                        "escalated".to_string(),
                        format!(
                            "{reason}. Max review rounds ({}) exceeded — escalating to supervisor.",
                            state.max_rounds
                        ),
                    )
                } else {
                    ("changes_requested".to_string(), reason)
                }
            }
            brehon_review::ThresholdResult::Rejected => {
                let reason = if collector.has_rejection() {
                    "Reviewer issued reject verdict".to_string()
                } else {
                    format!(
                        "Score {} <= 3 (fundamental issues)",
                        collector.min_score().map_or(0, |s| s.as_u8())
                    )
                };
                ("rejected".to_string(), reason)
            }
            brehon_review::ThresholdResult::NeedMoreReviewers => {
                ("collecting".to_string(), "Need more reviewers".to_string())
            }
        };

        // Step 3: Feedback consolidation using brehon-review
        let domain_submissions: Vec<brehon_review::panel::ReviewerSubmission> = submissions
            .iter()
            .map(|sub| {
                let findings: Vec<ReviewFinding> =
                    sub.findings.iter().map(|f| f.to_review_finding()).collect();
                brehon_review::panel::ReviewerSubmission {
                    reviewer_id: sub.reviewer.clone(),
                    session_id: brehon_types::SessionId::new(&sub.reviewer),
                    score: ReviewScore::new(sub.score),
                    verdict: parse_verdict(&sub.verdict),
                    findings,
                }
            })
            .collect();

        let consolidator = FeedbackConsolidator::new();
        let mut consolidated = consolidator.consolidate(&domain_submissions);
        consolidated
            .dissent
            .extend(ignored_negative_reviews.into_iter().map(|reason| {
                format!("Ignored unsupported negative review for threshold evaluation: {reason}")
            }));

        let avg = collector.average_score().unwrap_or(0.0);
        let min = collector.min_score().map_or(0, |s| s.as_u8());

        ConsolidatedReport {
            review_id: review_id.to_string(),
            task_id: task_id.to_string(),
            round: state.current_round,
            outcome,
            scores: Value::Object(scores_map),
            average_score: (avg * 10.0).round() / 10.0,
            min_score: min,
            approval_count: collector.approval_count(),
            threshold_result: format!("{threshold:?}"),
            threshold_reason,
            blocking: consolidated
                .blocking
                .iter()
                .map(StoredFinding::from_review_finding)
                .collect(),
            suggestions: consolidated
                .suggestions
                .iter()
                .map(StoredFinding::from_review_finding)
                .collect(),
            nitpicks: consolidated
                .nitpicks
                .iter()
                .map(StoredFinding::from_review_finding)
                .collect(),
            dissent: consolidated.dissent,
            evaluated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Build calibration from all stored review submissions across all tasks.
    pub(super) fn build_calibration(&self) -> brehon_review::calibration::ReviewerCalibration {
        let mut calibration = brehon_review::calibration::ReviewerCalibration::new();
        let Some(reviews_root) = reviews_dir() else {
            return calibration;
        };
        let Ok(entries) = std::fs::read_dir(&reviews_root) else {
            return calibration;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let tid = entry.file_name().to_string_lossy().to_string();
            if tid == "calibration.json" {
                continue;
            }
            let Some(state) = read_review_state(&tid) else {
                continue;
            };
            for round in 1..=state.current_round {
                let subs = read_round_submissions(&tid, round);
                for sub in &subs {
                    let is_approval = sub.verdict == "approved";
                    let is_rejection = sub.verdict == "rejected";
                    calibration.record_review(&sub.reviewer, sub.score, is_approval, is_rejection);
                }
            }
        }
        calibration
    }

    /// Persist calibration snapshot to disk.
    pub(super) fn persist_calibration(
        &self,
        calibration: &brehon_review::calibration::ReviewerCalibration,
    ) -> Option<StoredCalibration> {
        let global_avg = calibration.global_average();
        let outlier_threshold = 2.0;
        let mut entries = Vec::new();
        for (id, stats) in calibration.all_reviewers() {
            let is_outlier = global_avg
                .map(|avg| stats.is_outlier(avg, outlier_threshold))
                .unwrap_or(false);
            entries.push(StoredCalibrationEntry {
                reviewer_id: id.clone(),
                review_count: stats.review_count,
                average_score: stats.average_score(),
                std_deviation: stats.std_deviation(),
                approval_rate: stats.approval_rate(),
                approval_count: stats.approval_count,
                rejection_count: stats.rejection_count,
                changes_requested_count: stats.changes_requested_count,
                is_outlier,
            });
        }
        let snapshot = StoredCalibration {
            reviewers: entries,
            global_average: calibration.global_average(),
            global_std_deviation: calibration.global_std_deviation(),
            global_approval_rate: calibration.global_approval_rate(),
            outlier_threshold,
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        // Persist
        if let Some(root) = reviews_dir() {
            let _ = std::fs::create_dir_all(&root);
            let path = root.join("calibration.json");
            if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
                let _ = std::fs::write(path, json);
            }
        }
        Some(snapshot)
    }

    pub(super) fn format_consolidated_report(
        &self,
        task_id: &str,
        report: &ConsolidatedReport,
    ) -> String {
        let completion_mode = read_task_completion_mode(task_id);
        let completion_mode_str = completion_mode.as_str();
        let task_gate = task_status_for_review_outcome(&report.outcome).unwrap_or("unchanged");

        let gate_label = match task_gate {
            "approved" => "approved (awaiting supervisor action)",
            "changes_requested" => "changes_requested",
            _ => task_gate,
        };

        let mut msg = format!(
            "Review complete for task {task_id}\n\
             Review ID: {}\n\
             Outcome: {}\n\
             Round: {}/{}\n\
             Task gate now: {gate_label}\n\
             Completion mode: {completion_mode_str}\n\n\
             Scores:\n",
            report.review_id,
            report.outcome.to_uppercase(),
            report.round,
            self.config.policy.max_review_rounds
        );

        if let Some(scores) = report.scores.as_object() {
            for (reviewer, info) in scores {
                let score = info.get("score").and_then(|v| v.as_u64()).unwrap_or(0);
                let verdict = info.get("verdict").and_then(|v| v.as_str()).unwrap_or("?");
                msg.push_str(&format!("  {reviewer}: {score}/10 ({verdict})\n"));
            }
        }

        msg.push_str(&format!(
            "\nAverage: {:.1}/10 | Min: {}/10 | Approvals: {}/{}\n\
             Threshold: {} — {}\n",
            report.average_score,
            report.min_score,
            report.approval_count,
            report.scores.as_object().map_or(0, |m| m.len()),
            report.threshold_result,
            report.threshold_reason
        ));

        if !report.blocking.is_empty() {
            msg.push_str("\nBlocking Issues (must fix):\n");
            for (i, f) in report.blocking.iter().enumerate() {
                let loc = match (&f.file, f.line) {
                    (Some(file), Some(line)) => format!("[{file}:{line}] "),
                    _ => String::new(),
                };
                msg.push_str(&format!("  {}. {}{}\n", i + 1, loc, f.description));
                if let Some(ref sug) = f.suggestion {
                    msg.push_str(&format!("     Suggestion: {sug}\n"));
                }
            }
        }

        if !report.suggestions.is_empty() {
            msg.push_str("\nSuggestions:\n");
            for (i, f) in report.suggestions.iter().enumerate() {
                let loc = match (&f.file, f.line) {
                    (Some(file), Some(line)) => format!("[{file}:{line}] "),
                    _ => String::new(),
                };
                msg.push_str(&format!("  {}. {}{}\n", i + 1, loc, f.description));
            }
        }

        if !report.dissent.is_empty() {
            msg.push_str("\nDissent:\n");
            for d in &report.dissent {
                msg.push_str(&format!("  - {d}\n"));
            }
        }

        match report.outcome.as_str() {
            "approved" => match completion_mode {
                TaskCompletionMode::Merge => {
                    if merge_target_requires_epic_integration(task_id) {
                        let merge_target = read_task_merge_target(task_id)
                            .unwrap_or_else(|| "epic branch".to_string());
                        msg.push_str(&format!(
                                "\nTask approved (awaiting merge-target integration). The task status is now 'approved'. \
                                 Merge target: {merge_target}.\n\
                                 SUPERVISOR: You must integrate the reviewed commit into '{merge_target}' \
                                 yourself — workers cannot perform the terminal integration step after approval. Run:\n  \
                                 task action=integrate id={task_id}\n\
                                 This lands the task on its parent integration branch. Only a top-level container close may merge to main."
                            ));
                    } else {
                        let merge_target = read_task_merge_target(task_id)
                            .or_else(detect_default_branch)
                            .unwrap_or_else(|| "main".to_string());
                        msg.push_str(&format!(
                                "\nTask approved (not yet merged). The task status is now 'approved'. \
                                 Merge target: {merge_target}.\n\
                                 SUPERVISOR: You must perform the terminal integration — workers cannot. \
                                 Integrate the reviewed commit into '{merge_target}', then run:\n  \
                                 task action=close id={task_id}\n\
                                 Completion mode is merge. This will verify the reviewed commit is on \
                                 {merge_target} and mark the task as 'merged'."
                            ));
                    }
                }
                TaskCompletionMode::Close => {
                    msg.push_str(&format!(
                            "\nTask approved (awaiting close). The task status is now 'approved'. \
                             SUPERVISOR: You must perform the terminal close — workers cannot. Run:\n  \
                             task action=close id={task_id}\n\
                             Completion mode is close. This will mark it as 'closed' without a merge."
                        ));
                }
            },
            "changes_requested" => {
                msg.push_str(
                    "\nTask gate moved to 'changes_requested'. The structured review_feedback \
                     is now attached to the task and routed to the current worker assignee. \
                     After fixes, call verification action=request_review again for the next round.",
                );
            }
            "rejected" => {
                msg.push_str(
                    "\nTask rejected (changes_requested). The task gate remains blocked. \
                     Decide: reassign, rework with a new approach, or close as wontfix.",
                );
            }
            "escalated" => {
                msg.push_str(
                    "\nMax review rounds exceeded (escalated). The task gate remains blocked \
                     via 'changes_requested'. Do not approve by override:\n\
                     - verification action=reset_rounds task_id=<id> reason=\"...\"\n\
                     - verification action=override task_id=<id> verdict=needs_revision reason=\"...\"\n\
                     - Or reject and reassign.",
                );
            }
            _ => {}
        }

        msg
    }
}

#[async_trait]
impl Tool for VerificationTool {
    fn name(&self) -> &str {
        "verification"
    }

    fn description(&self) -> &str {
        "Review coordination — request reviews, submit scores, check status, mark negative override outcomes, and reset exhausted review cycles."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: submit_review, review_status, calibration_stats for reviewers. Supervisor/maintenance actions: request_review, override, reset_rounds, reseat_panel, reassign_panel, release_panel. override cannot approve work."
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID (for request_review, review_status, override, reset_rounds, reseat_panel, reassign_panel, release_panel)"
                },
                "review_id": {
                    "type": "string",
                    "description": "Review ID returned by request_review (for submit_review)"
                },
                "score": {
                    "type": "integer",
                    "description": "Review score 1-10 (for submit_review)"
                },
                "verdict": {
                    "type": "string",
                    "description": "Verdict: approved, needs_revision, rejected for submit_review. Override only accepts needs_revision or rejected; override cannot approve work."
                },
                "summary": {
                    "type": "string",
                    "description": "Review summary text"
                },
                "findings": {
                    "type": "string",
                    "description": "JSON array of findings [{description, file, line, severity, suggestion}]"
                },
                "title": {
                    "type": "string",
                    "description": "Optional task title override (for request_review). Usually omit; Brehon reads the task title."
                },
                "description": {
                    "type": "string",
                    "description": "Optional task description override (for request_review). Usually omit; Brehon reads the task description."
                },
                "commit": {
                    "type": "string",
                    "description": "Deprecated escape hatch for request_review. For merge-mode tasks, omit commit; Brehon uses the task's authoritative latest_commit and rejects mismatches."
                },
                "context": {
                    "type": "string",
                    "description": "Additional context (for request_review)"
                },
                "reason": {
                    "type": "string",
                    "description": "Supervisor reason (for override, reset_rounds, release_panel)"
                },
                "panel_id": {
                    "type": "string",
                    "description": "Optional configured panel id to use when reseating a collecting review onto a leased panel"
                },
                "reviewer": {
                    "type": "string",
                    "description": "Your agent name as reviewer (for submit_review). Required when BREHON_AGENT_NAME env var is not set."
                },
                "requested_by": {
                    "type": "string",
                    "description": "Your agent name (for request_review, override, reset_rounds). Falls back to BREHON_AGENT_NAME env var."
                },
                "role": {
                    "type": "string",
                    "description": "Your agent role (for override, reset_rounds). Falls back to BREHON_AGENT_ROLE env var."
                },
                "reviewer_id": {
                    "type": "string",
                    "description": "Reviewer ID filter (for calibration_stats)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        match action {
            "request_review" => self.handle_request_review(&args).await,
            "submit_review" => self.handle_submit_review(&args).await,
            "review_status" => self.handle_review_status(&args).await,
            "override" => self.handle_override(&args).await,
            "reset_rounds" => self.handle_reset_rounds(&args).await,
            "reseat_panel" => self.handle_reseat_panel(&args).await,
            "reassign_panel" => self.handle_reassign_panel(&args).await,
            "release_panel" => self.handle_release_panel(&args).await,
            "calibration_stats" => self.handle_calibration_stats(&args).await,
            _ => Ok(error_result(format!(
                "Unknown verification action: {action}. \
                 Available: request_review, submit_review, review_status, override, reset_rounds, reseat_panel, \
                 reassign_panel, release_panel, calibration_stats"
            ))),
        }
    }
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
