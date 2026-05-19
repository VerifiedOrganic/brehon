//! Materialized view persistence.
//!
//! Handles updating and retrieving task and review state views.

use chrono::Utc;
use fjall::{Batch, Keyspace, PartitionHandle, PersistMode};
use std::collections::HashMap;

use brehon_types::{
    Event, EventId, EventKind, ReviewStatus, ReviewView, TaskStatus, TaskView, ViewOperation,
    ViewType, ViewUpdate,
};

use crate::keys::*;
use crate::store::StoreError;

pub struct ViewManager {
    pub views: PartitionHandle,
    keyspace: Keyspace,
}

impl ViewManager {
    pub fn new(keyspace: Keyspace, views: PartitionHandle) -> Self {
        Self { views, keyspace }
    }

    pub fn rebuild_views_from_events(&self, events: &PartitionHandle) -> Result<usize, StoreError> {
        let mut task_views: HashMap<String, TaskView> = HashMap::new();
        let mut review_views: HashMap<String, ReviewView> = HashMap::new();
        let mut review_to_task: HashMap<String, String> = HashMap::new();

        let iter = events.prefix(b"log:");
        for result in iter {
            let (_key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            let envelope = serde_json::from_slice::<brehon_types::EventEnvelope>(&value)?;
            Self::apply_event_to_views(
                &mut task_views,
                &mut review_views,
                &mut review_to_task,
                &envelope.event,
                envelope.event_id,
            );
        }

        let mut batch = self.keyspace.batch().durability(Some(PersistMode::SyncAll));
        self.stage_view_partition_deletions(&mut batch)?;
        let rebuilt_count = self.persist_rebuilt_views(
            &mut batch,
            task_views.into_values().collect(),
            review_views.into_values().collect(),
        )?;
        batch.commit()?;

        Ok(rebuilt_count)
    }

    pub fn validate_views(&self, expected_last_event_id: u64) -> Result<bool, StoreError> {
        let iter = self.views.iter();
        for result in iter {
            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            if key.starts_with(b"view:task:") {
                let view: TaskView = match serde_json::from_slice(&value) {
                    Ok(view) => view,
                    Err(_) => return Ok(false),
                };
                if view.last_event_id > expected_last_event_id {
                    return Ok(false);
                }
            } else if key.starts_with(b"view:review:") {
                let view: ReviewView = match serde_json::from_slice(&value) {
                    Ok(view) => view,
                    Err(_) => return Ok(false),
                };
                if view.last_event_id > expected_last_event_id {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    fn stage_view_partition_deletions(&self, batch: &mut Batch) -> Result<(), StoreError> {
        let mut view_keys = Vec::new();
        let iter = self.views.iter();
        for result in iter {
            let (key, _value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;
            if key.starts_with(b"view:task:") || key.starts_with(b"view:review:") {
                view_keys.push(key.to_vec());
            }
        }

        for key in view_keys {
            batch.remove(&self.views, key);
        }

        Ok(())
    }

    fn persist_rebuilt_views(
        &self,
        batch: &mut Batch,
        task_views: Vec<TaskView>,
        review_views: Vec<ReviewView>,
    ) -> Result<usize, StoreError> {
        let mut count = 0usize;

        for view in task_views {
            let key = task_view_key(&view.task_id);
            let value = serde_json::to_vec(&view)?;
            batch.insert(&self.views, &key, &value);
            count = count.saturating_add(1);
        }

        for view in review_views {
            let key = review_view_key(&view.review_id);
            let value = serde_json::to_vec(&view)?;
            batch.insert(&self.views, &key, &value);
            count = count.saturating_add(1);
        }

        Ok(count)
    }

    fn task_id_for_review(
        review_to_task: &std::collections::HashMap<String, String>,
        review_views: &std::collections::HashMap<String, ReviewView>,
        review_id: &str,
    ) -> Option<String> {
        if let Some(task_id) = review_to_task.get(review_id) {
            return Some(task_id.clone());
        }

        match review_views.get(review_id) {
            Some(view) if !view.task_id.is_empty() => Some(view.task_id.clone()),
            _ => None,
        }
    }

    fn apply_event_to_views(
        task_views: &mut HashMap<String, TaskView>,
        review_views: &mut HashMap<String, ReviewView>,
        review_to_task: &mut HashMap<String, String>,
        event: &Event,
        event_id: EventId,
    ) {
        match &event.kind {
            EventKind::AgentSpawned { .. } => {}
            EventKind::AgentDied { .. } => {}
            EventKind::PromptSent { .. } => {}
            EventKind::PromptCancelled { .. } => {}
            EventKind::ResponseReceived { .. } => {}
            EventKind::PermissionRequested { .. } => {}
            EventKind::PermissionResolved { .. } => {}
            EventKind::OperationStarted { .. } => {}
            EventKind::OperationCompleted { .. } => {}
            EventKind::TaskCreated { task_id } => {
                task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| TaskView {
                        task_id: task_id.clone(),
                        status: TaskStatus::Pending,
                        assignee: None,
                        session_id: None,
                        branch: None,
                        review_rounds: 0,
                        last_event_id: event_id.as_u64(),
                        updated_at: event.timestamp,
                    });
            }
            EventKind::TaskAssigned { task_id, agent_id } => {
                let view = task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| Self::default_task_view(task_id));
                view.status = TaskStatus::Assigned;
                view.assignee = Some(agent_id.clone());
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::TaskCompleted { task_id } => {
                let view = task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| Self::default_task_view(task_id));
                view.status = TaskStatus::InReview;
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::RunCreated { .. } => {}
            EventKind::RunClaimed { .. } => {}
            EventKind::RunClaimRenewed { .. } => {}
            EventKind::RunStarted { .. } => {}
            EventKind::RunActivityObserved { .. } => {}
            EventKind::RunReleased { .. } => {}
            EventKind::RunRetryQueued { .. } => {}
            EventKind::RunCompleted { .. } => {}
            EventKind::RunFailed { .. } => {}
            EventKind::RunAbandoned { .. } => {}
            EventKind::StaleRunMutationRejected { .. } => {}
            EventKind::ProofBundleCreated { .. } => {}
            EventKind::ProofCommandRecorded { .. } => {}
            EventKind::ProofCheckRecorded { .. } => {}
            EventKind::ProofReviewLinked { .. } => {}
            EventKind::ProofIntegrationRecorded { .. } => {}
            EventKind::ProofDecisionRecorded { .. } => {}
            EventKind::ProofBlockerRecorded { .. } => {}
            EventKind::ProofBundleFinalized { .. } => {}
            EventKind::MergePrepared { task_id, branch } => {
                let view = task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| Self::default_task_view(task_id));
                view.branch = Some(branch.clone());
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::MergeAborted { task_id, .. } => {
                let view = task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| Self::default_task_view(task_id));
                view.status = TaskStatus::InProgress;
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::MergeCommitted { task_id } => {
                let view = task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| Self::default_task_view(task_id));
                view.status = TaskStatus::Merged;
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::ReviewRequested { task_id, review_id } => {
                let task_view = task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| Self::default_task_view(task_id));
                task_view.status = TaskStatus::InReview;
                task_view.review_rounds = task_view.review_rounds.saturating_add(1);
                task_view.last_event_id = event_id.as_u64();
                task_view.updated_at = event.timestamp;

                let review_view = review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| Self::default_review_view(review_id));
                review_view.task_id = task_id.clone();
                review_view.status = ReviewStatus::Pending;
                review_view.round = review_view.round.saturating_add(1);
                review_view.scores.clear();
                review_view.panel.clear();
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;

                review_to_task.insert(review_id.clone(), task_id.clone());
            }
            EventKind::ReviewScoreReceived {
                review_id,
                reviewer_id,
                score,
            } => {
                let review_view = review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| Self::default_review_view(review_id));
                review_view.status = ReviewStatus::InProgress;
                review_view.scores.push((reviewer_id.clone(), *score));
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;
            }
            EventKind::ReviewApproved { review_id } => {
                let review_view = review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| Self::default_review_view(review_id));
                review_view.status = ReviewStatus::Completed;
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;

                if let Some(task_id) =
                    Self::task_id_for_review(review_to_task, review_views, review_id)
                {
                    let task_view = task_views
                        .entry(task_id.clone())
                        .or_insert_with(|| Self::default_task_view(&task_id));
                    task_view.status = TaskStatus::Approved;
                    task_view.last_event_id = event_id.as_u64();
                    task_view.updated_at = event.timestamp;
                }
            }
            EventKind::ReviewRejected { review_id } => {
                let review_view = review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| Self::default_review_view(review_id));
                review_view.status = ReviewStatus::Completed;
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;

                if let Some(task_id) =
                    Self::task_id_for_review(review_to_task, review_views, review_id)
                {
                    let task_view = task_views
                        .entry(task_id.clone())
                        .or_insert_with(|| Self::default_task_view(&task_id));
                    task_view.status = TaskStatus::ChangesRequested;
                    task_view.last_event_id = event_id.as_u64();
                    task_view.updated_at = event.timestamp;
                }
            }
            EventKind::ReviewChangesRequested { review_id } => {
                let review_view = review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| Self::default_review_view(review_id));
                review_view.status = ReviewStatus::Completed;
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;

                if let Some(task_id) =
                    Self::task_id_for_review(review_to_task, review_views, review_id)
                {
                    let task_view = task_views
                        .entry(task_id.clone())
                        .or_insert_with(|| Self::default_task_view(&task_id));
                    task_view.status = TaskStatus::ChangesRequested;
                    task_view.last_event_id = event_id.as_u64();
                    task_view.updated_at = event.timestamp;
                }
            }
            EventKind::EpicBranchCreated { .. } => {}
            EventKind::SubtaskBranchCreated { .. } => {}
            EventKind::SubtaskIntegrated { .. } => {}
            EventKind::NudgeSent { .. } => {}
            EventKind::NudgeAcknowledged { .. } => {}
            EventKind::NudgeActedOn { .. } => {}
            EventKind::NudgeTimedOut { .. } => {}
            EventKind::MemoryCreated { .. } | EventKind::MemoryDeleted { .. } => {}
            EventKind::StuckDetected { .. } => {}
            EventKind::EscalationTriggered { .. } => {}
            EventKind::SystemDraining { .. } => {}
            EventKind::WorkerReassigned {
                old_worker: _,
                new_worker,
                task_id,
                ..
            } => {
                let view = task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| Self::default_task_view(task_id));
                view.assignee = Some(new_worker.clone());
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::FeedbackTriggerDetected { .. }
            | EventKind::FeedbackBriefBuilt { .. }
            | EventKind::FeedbackTurnStarted { .. }
            | EventKind::FeedbackOutcomeReceived { .. }
            | EventKind::FeedbackOutcomeValidated { .. }
            | EventKind::FeedbackOutcomeRejected { .. }
            | EventKind::FeedbackDecisionRecorded { .. }
            | EventKind::FeedbackApplied { .. }
            | EventKind::FeedbackFailed { .. } => {
                // Feedback events are projected by the feedback module
                // (Phase 6) rather than affecting task/review views here.
            }
        }
    }

    pub fn apply_update(&self, update: &ViewUpdate) -> Result<(), StoreError> {
        match update.view_type {
            ViewType::Task => self.apply_task_update(&update.key, &update.operation),
            ViewType::Review => self.apply_review_update(&update.key, &update.operation),
            ViewType::Agent => self.apply_agent_update(&update.key, &update.operation),
            ViewType::Budget => self.apply_budget_update(&update.key, &update.operation),
        }
    }

    pub fn stage_updates(
        &self,
        updates: &[ViewUpdate],
        batch: &mut Batch,
    ) -> Result<(), StoreError> {
        let mut staged_task_views: HashMap<String, TaskView> = HashMap::new();
        let mut staged_review_views: HashMap<String, ReviewView> = HashMap::new();

        for update in updates {
            match update.view_type {
                ViewType::Task => {
                    let view = if let Some(view) = staged_task_views.get_mut(&update.key) {
                        view
                    } else {
                        let loaded = self
                            .get_task_view(&update.key)?
                            .unwrap_or_else(|| Self::default_task_view(&update.key));
                        staged_task_views.insert(update.key.clone(), loaded);
                        staged_task_views
                            .get_mut(&update.key)
                            .expect("inserted task view must be present")
                    };
                    Self::apply_task_operation(view, &update.operation);
                    view.updated_at = Utc::now();
                }
                ViewType::Review => {
                    let view = if let Some(view) = staged_review_views.get_mut(&update.key) {
                        view
                    } else {
                        let loaded = self
                            .get_review_view(&update.key)?
                            .unwrap_or_else(|| Self::default_review_view(&update.key));
                        staged_review_views.insert(update.key.clone(), loaded);
                        staged_review_views
                            .get_mut(&update.key)
                            .expect("inserted review view must be present")
                    };
                    Self::apply_review_operation(view, &update.operation);
                    view.updated_at = Utc::now();
                }
                ViewType::Agent => {}
                ViewType::Budget => {}
            }
        }

        for view in staged_task_views.values() {
            let key = task_view_key(&view.task_id);
            let value = serde_json::to_vec(view)?;
            batch.insert(&self.views, key, value);
        }
        for view in staged_review_views.values() {
            let key = review_view_key(&view.review_id);
            let value = serde_json::to_vec(view)?;
            batch.insert(&self.views, key, value);
        }

        Ok(())
    }

    fn apply_task_update(
        &self,
        task_id: &str,
        operation: &ViewOperation,
    ) -> Result<(), StoreError> {
        let mut view = self
            .get_task_view(task_id)?
            .unwrap_or_else(|| Self::default_task_view(task_id));
        Self::apply_task_operation(&mut view, operation);

        view.updated_at = Utc::now();
        self.save_task_view(&view)
    }

    fn apply_review_update(
        &self,
        review_id: &str,
        operation: &ViewOperation,
    ) -> Result<(), StoreError> {
        let mut view = self
            .get_review_view(review_id)?
            .unwrap_or_else(|| Self::default_review_view(review_id));
        Self::apply_review_operation(&mut view, operation);

        view.updated_at = Utc::now();
        self.save_review_view(&view)
    }

    fn apply_agent_update(&self, _key: &str, _operation: &ViewOperation) -> Result<(), StoreError> {
        Ok(())
    }

    fn apply_budget_update(
        &self,
        _key: &str,
        _operation: &ViewOperation,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn get_task_view(&self, task_id: &str) -> Result<Option<TaskView>, StoreError> {
        let key = task_view_key(task_id);
        match self.views.get(&key)? {
            Some(bytes) => {
                let view = serde_json::from_slice(&bytes)?;
                Ok(Some(view))
            }
            None => Ok(None),
        }
    }

    pub fn get_review_view(&self, review_id: &str) -> Result<Option<ReviewView>, StoreError> {
        let key = review_view_key(review_id);
        match self.views.get(&key)? {
            Some(bytes) => {
                let view = serde_json::from_slice(&bytes)?;
                Ok(Some(view))
            }
            None => Ok(None),
        }
    }

    fn save_task_view(&self, view: &TaskView) -> Result<(), StoreError> {
        let key = task_view_key(&view.task_id);
        let value = serde_json::to_vec(view)?;
        self.views.insert(&key, &value)?;
        Ok(())
    }

    fn save_review_view(&self, view: &ReviewView) -> Result<(), StoreError> {
        let key = review_view_key(&view.review_id);
        let value = serde_json::to_vec(view)?;
        self.views.insert(&key, &value)?;
        Ok(())
    }

    fn default_task_view(task_id: &str) -> TaskView {
        TaskView {
            task_id: task_id.to_string(),
            status: TaskStatus::Pending,
            assignee: None,
            session_id: None,
            branch: None,
            review_rounds: 0,
            last_event_id: 0,
            updated_at: Utc::now(),
        }
    }

    fn default_review_view(review_id: &str) -> ReviewView {
        ReviewView {
            review_id: review_id.to_string(),
            task_id: String::new(),
            status: ReviewStatus::Pending,
            round: 0,
            scores: Vec::new(),
            panel: Vec::new(),
            last_event_id: 0,
            updated_at: Utc::now(),
        }
    }

    fn apply_task_operation(view: &mut TaskView, operation: &ViewOperation) {
        match operation {
            ViewOperation::Set { field, value } => match field.as_str() {
                "status" => {
                    view.status = serde_json::from_str(value).unwrap_or(TaskStatus::Pending);
                }
                "assignee" => {
                    view.assignee = Some(value.clone());
                }
                "session_id" => {
                    view.session_id = Some(value.clone());
                }
                "branch" => {
                    view.branch = Some(value.clone());
                }
                _ => {}
            },
            ViewOperation::Increment { field, amount } => {
                if field == "review_rounds" {
                    if *amount > 0 {
                        view.review_rounds = view.review_rounds.saturating_add(*amount as u32);
                    } else {
                        view.review_rounds = view.review_rounds.saturating_sub((-amount) as u32);
                    }
                }
            }
            ViewOperation::Remove { field, value } => {
                if field == "assignee" && view.assignee.as_deref() == Some(value.as_str()) {
                    view.assignee = None;
                }
                if field == "session_id" && view.session_id.as_deref() == Some(value.as_str()) {
                    view.session_id = None;
                }
                if field == "branch" && view.branch.as_deref() == Some(value.as_str()) {
                    view.branch = None;
                }
            }
            ViewOperation::Append { field, value } => {
                let _ = (field, value);
            }
        }
    }

    fn apply_review_operation(view: &mut ReviewView, operation: &ViewOperation) {
        match operation {
            ViewOperation::Set { field, value } => match field.as_str() {
                "status" => {
                    view.status = serde_json::from_str(value).unwrap_or(ReviewStatus::Pending);
                }
                "task_id" => {
                    view.task_id = value.clone();
                }
                "round" => {
                    if let Ok(r) = value.parse() {
                        view.round = r;
                    }
                }
                _ => {}
            },
            ViewOperation::Append { field, value } => {
                if field == "scores" {
                    let parts: Vec<&str> = value.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        if let Ok(score) = parts[1].parse() {
                            view.scores.push((parts[0].to_string(), score));
                        }
                    }
                }
                if field == "panel" {
                    view.panel.push(value.clone());
                }
            }
            ViewOperation::Increment { field, amount } => {
                if field == "round" {
                    if *amount > 0 {
                        view.round = view.round.saturating_add(*amount as u32);
                    } else {
                        view.round = view.round.saturating_sub((-amount) as u32);
                    }
                }
            }
            ViewOperation::Remove { field, value } => {
                let _ = (field, value);
            }
        }
    }

    pub fn list_orphaned_tasks(&self) -> Result<Vec<TaskView>, StoreError> {
        let mut orphaned = Vec::new();

        let prefix = b"view:task:";

        let iter = self.views.iter();
        for result in iter {
            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            if !key.starts_with(prefix) {
                continue;
            }

            let view: TaskView = serde_json::from_slice(&value)?;
            if view.status == TaskStatus::InProgress && view.session_id.is_none() {
                orphaned.push(view);
            }
        }

        Ok(orphaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_operation_serialization() -> Result<(), serde_json::Error> {
        let op = ViewOperation::Set {
            field: "status".to_string(),
            value: "InProgress".to_string(),
        };
        let json = serde_json::to_string(&op)?;
        let parsed: ViewOperation = serde_json::from_str(&json)?;
        assert_eq!(op, parsed);
        Ok(())
    }

    #[test]
    fn test_task_view_serialization() -> Result<(), serde_json::Error> {
        let view = TaskView {
            task_id: "T001".to_string(),
            status: TaskStatus::InProgress,
            assignee: Some("agent-1".to_string()),
            session_id: None,
            branch: None,
            review_rounds: 0,
            last_event_id: 42,
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&view)?;
        let parsed: TaskView = serde_json::from_str(&json)?;
        assert_eq!(view.task_id, parsed.task_id);
        Ok(())
    }
}
