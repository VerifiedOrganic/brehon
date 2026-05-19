//! Recovery scan for detecting incomplete states.
//!
//! On startup, scans for:
//! - Tasks with `InProgress` status but no corresponding active agent
//! - Prepared merges without commit
//! - Expired review claims

use fjall::PartitionHandle;

use crate::store::StoreError;
use crate::views::ViewManager;
use brehon_types::{QueueClaim, TaskStatus, TaskView, ViewOperation, ViewType, ViewUpdate};

#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub orphaned_tasks: Vec<TaskView>,
    pub prepared_merges: Vec<String>,
    pub expired_claims: Vec<String>,
    pub recovered_count: usize,
}

impl Default for RecoveryReport {
    fn default() -> Self {
        Self::new()
    }
}

impl RecoveryReport {
    pub fn new() -> Self {
        Self {
            orphaned_tasks: Vec::new(),
            prepared_merges: Vec::new(),
            expired_claims: Vec::new(),
            recovered_count: 0,
        }
    }

    pub fn has_issues(&self) -> bool {
        !self.orphaned_tasks.is_empty()
            || !self.prepared_merges.is_empty()
            || !self.expired_claims.is_empty()
    }
}

pub struct RecoveryScanner {
    events: PartitionHandle,
    views: ViewManager,
    queue: PartitionHandle,
    active_lease_epoch: String,
    active_lease_elapsed_ms: u64,
}

impl RecoveryScanner {
    pub fn new(
        events: PartitionHandle,
        views: ViewManager,
        queue: PartitionHandle,
        active_lease_epoch: impl Into<String>,
        active_lease_elapsed_ms: u64,
    ) -> Self {
        Self {
            events,
            views,
            queue,
            active_lease_epoch: active_lease_epoch.into(),
            active_lease_elapsed_ms,
        }
    }

    pub fn scan(&self) -> Result<RecoveryReport, StoreError> {
        let mut report = RecoveryReport::new();

        let orphaned_tasks = self.find_orphaned_tasks()?;
        let prepared_merges = self.find_prepared_merges()?;
        let expired_claims = self.find_expired_claims()?;

        let recovered_orphaned = self.clean_orphaned_tasks(&orphaned_tasks)?;
        let recovered_merges = self.clean_prepared_merges(&prepared_merges)?;
        let recovered_claims = self.clean_expired_claims(&expired_claims)?;

        report.recovered_count = recovered_orphaned + recovered_merges + recovered_claims;

        report.orphaned_tasks = self.find_orphaned_tasks()?;
        report.prepared_merges = self.find_prepared_merges()?;
        report.expired_claims = self
            .find_expired_claims()?
            .into_iter()
            .map(|claim| claim.claim_id.as_str().to_string())
            .collect();

        Ok(report)
    }

    fn find_orphaned_tasks(&self) -> Result<Vec<TaskView>, StoreError> {
        let mut orphaned = Vec::new();

        let tasks = self.views.list_orphaned_tasks()?;

        for task in tasks {
            if task.status == TaskStatus::InProgress && task.session_id.is_none() {
                orphaned.push(task);
            }
        }

        Ok(orphaned)
    }

    fn find_prepared_merges(&self) -> Result<Vec<String>, StoreError> {
        let mut prepared = Vec::new();
        let mut prepared_tasks: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut committed_tasks: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut aborted_tasks: std::collections::HashSet<String> = std::collections::HashSet::new();

        let prefix = b"log:";

        let iter = self.events.iter();
        for result in iter {
            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            if !key.starts_with(prefix) {
                continue;
            }

            let envelope: brehon_types::EventEnvelope = match serde_json::from_slice(&value) {
                Ok(e) => e,
                Err(_) => continue,
            };

            match &envelope.event.kind {
                brehon_types::EventKind::MergePrepared { task_id, .. } => {
                    prepared_tasks.insert(task_id.clone());
                }
                brehon_types::EventKind::MergeCommitted { task_id } => {
                    committed_tasks.insert(task_id.clone());
                }
                brehon_types::EventKind::MergeAborted { task_id, .. } => {
                    aborted_tasks.insert(task_id.clone());
                }
                _ => {}
            }
        }

        for task_id in prepared_tasks {
            if !committed_tasks.contains(&task_id) && !aborted_tasks.contains(&task_id) {
                let has_branch_state = self
                    .views
                    .get_task_view(&task_id)?
                    .is_some_and(|view| view.branch.is_some());
                if has_branch_state {
                    prepared.push(task_id);
                }
            }
        }

        Ok(prepared)
    }

    fn find_expired_claims(&self) -> Result<Vec<QueueClaim>, StoreError> {
        let mut expired = Vec::new();
        let prefix = b"lease:";

        let iter = self.queue.iter();
        for result in iter {
            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            if !key.starts_with(prefix) {
                continue;
            }

            let claim: brehon_types::QueueClaim = match serde_json::from_slice(&value) {
                Ok(c) => c,
                Err(e) => {
                    let key_str = String::from_utf8_lossy(&key);
                    tracing::warn!(%key_str, error = %e, "Skipping malformed lease entry during recovery");
                    continue;
                }
            };

            if self.claim_is_expired(&claim) {
                expired.push(claim);
            }
        }

        Ok(expired)
    }

    fn claim_is_expired(&self, claim: &QueueClaim) -> bool {
        if let Some(epoch) = claim.lease_epoch.as_deref() {
            if epoch != self.active_lease_epoch {
                return true;
            }

            if let Some(monotonic_deadline_ms) = claim.monotonic_deadline_ms {
                return self.active_lease_elapsed_ms >= monotonic_deadline_ms;
            }
        }

        claim.is_expired()
    }

    fn clean_orphaned_tasks(&self, orphaned_tasks: &[TaskView]) -> Result<usize, StoreError> {
        for task in orphaned_tasks {
            self.views.apply_update(&ViewUpdate {
                view_type: ViewType::Task,
                key: task.task_id.clone(),
                operation: ViewOperation::Set {
                    field: "status".to_string(),
                    value: serde_json::to_string(&TaskStatus::Pending)?,
                },
            })?;

            if let Some(assignee) = &task.assignee {
                self.views.apply_update(&ViewUpdate {
                    view_type: ViewType::Task,
                    key: task.task_id.clone(),
                    operation: ViewOperation::Remove {
                        field: "assignee".to_string(),
                        value: assignee.clone(),
                    },
                })?;
            }

            if let Some(session_id) = &task.session_id {
                self.views.apply_update(&ViewUpdate {
                    view_type: ViewType::Task,
                    key: task.task_id.clone(),
                    operation: ViewOperation::Remove {
                        field: "session_id".to_string(),
                        value: session_id.clone(),
                    },
                })?;
            }
        }

        Ok(orphaned_tasks.len())
    }

    fn clean_prepared_merges(&self, prepared_merges: &[String]) -> Result<usize, StoreError> {
        let mut recovered = 0usize;

        for task_id in prepared_merges {
            let Some(task_view) = self.views.get_task_view(task_id)? else {
                continue;
            };
            let Some(branch) = task_view.branch else {
                continue;
            };

            self.views.apply_update(&ViewUpdate {
                view_type: ViewType::Task,
                key: task_id.clone(),
                operation: ViewOperation::Remove {
                    field: "branch".to_string(),
                    value: branch,
                },
            })?;
            recovered = recovered.saturating_add(1);
        }

        Ok(recovered)
    }

    fn clean_expired_claims(&self, expired_claims: &[QueueClaim]) -> Result<usize, StoreError> {
        for claim in expired_claims {
            let lease_key = format!("lease:{}", claim.claim_id.as_str());
            let claimed_key = format!("claimed:{}:{}", claim.queue, claim.item_id);
            self.queue.remove(claimed_key.as_bytes())?;
            self.queue.remove(lease_key.as_bytes())?;
        }
        Ok(expired_claims.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::TaskStatus;
    use chrono::Utc;

    #[test]
    fn test_recovery_report_defaults() {
        let report = RecoveryReport::new();
        assert!(report.orphaned_tasks.is_empty());
        assert!(report.prepared_merges.is_empty());
        assert!(report.expired_claims.is_empty());
        assert!(!report.has_issues());
    }

    #[test]
    fn test_recovery_report_with_issues() {
        let mut report = RecoveryReport::new();
        report.orphaned_tasks.push(TaskView {
            task_id: "T001".to_string(),
            status: TaskStatus::InProgress,
            assignee: None,
            session_id: None,
            branch: None,
            review_rounds: 0,
            last_event_id: 42,
            updated_at: Utc::now(),
        });
        assert!(report.has_issues());
    }
}
