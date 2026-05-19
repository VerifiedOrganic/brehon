//! In-memory durable run store implementation for tests.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use parking_lot::RwLock;

use brehon_ports::{
    ClaimRelease, ClaimRequest, RetryAttemptRequest, RetryAttemptStarted, RunCompletion,
    RunContinuation, RunFailure, RunRetry, RunStore, RunStoreError, RunStoreResult,
};
use brehon_types::{ClaimGeneration, RunId, RunRecord, RunStatus, TaskId};

const DEFAULT_RENEWAL_SECONDS: i64 = 60;

/// In-memory `RunStore` implementation for deterministic tests.
#[derive(Debug, Default)]
pub struct InMemoryRunStore {
    records: RwLock<HashMap<RunId, RunRecord>>,
}

impl InMemoryRunStore {
    /// Create an empty in-memory run store.
    pub fn new() -> Self {
        Self::default()
    }

    fn active_duplicate_for(
        record: &RunRecord,
        records: &HashMap<RunId, RunRecord>,
    ) -> Option<RunId> {
        records
            .values()
            .find(|existing| {
                existing.run_id != record.run_id
                    && existing.task_id == record.task_id
                    && existing.role == record.role
                    && existing.is_active()
            })
            .map(|existing| existing.run_id.clone())
    }

    fn load_required(
        records: &HashMap<RunId, RunRecord>,
        run_id: &RunId,
    ) -> RunStoreResult<RunRecord> {
        records
            .get(run_id)
            .cloned()
            .ok_or_else(|| RunStoreError::NotFound {
                run_id: run_id.clone(),
            })
    }

    fn ensure_generation(record: &RunRecord, supplied: ClaimGeneration) -> RunStoreResult<()> {
        if record.claim_generation == supplied {
            Ok(())
        } else {
            Err(RunStoreError::StaleGeneration {
                run_id: record.run_id.clone(),
                expected: record.claim_generation,
                actual: supplied,
            })
        }
    }

    fn ensure_unexpired(record: &RunRecord) -> RunStoreResult<()> {
        if record.status.has_active_claim() && record.claim_is_expired_at(Utc::now()) {
            Err(RunStoreError::LeaseExpired {
                run_id: record.run_id.clone(),
                generation: record.claim_generation,
            })
        } else {
            Ok(())
        }
    }

    fn can_claim(record: &RunRecord) -> bool {
        matches!(record.status, RunStatus::Created | RunStatus::Released)
            || (record.status == RunStatus::RetryQueued && record.retry_is_due_at(Utc::now()))
            || (record.status.has_active_claim() && record.claim_is_expired_at(Utc::now()))
    }

    fn clear_claim(record: &mut RunRecord) {
        record.claim_owner = None;
        record.session_id = None;
        record.lease_expires_at = None;
        record.claimed_at = None;
    }
}

#[async_trait]
impl RunStore for InMemoryRunStore {
    async fn create_run(&self, mut record: RunRecord) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        if records.contains_key(&record.run_id) {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: record.status,
            });
        }

        if record.is_active() {
            if let Some(existing_run_id) = Self::active_duplicate_for(&record, &records) {
                return Err(RunStoreError::DuplicateActiveClaim {
                    task_id: record.task_id,
                    role: record.role,
                    existing_run_id,
                });
            }
        }

        record.updated_at = record.created_at;
        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn claim_run(&self, request: ClaimRequest) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        let mut record = Self::load_required(&records, &request.run_id)?;

        if record.status == RunStatus::RetryQueued && !record.retry_is_due_at(request.claimed_at) {
            return Err(RunStoreError::RetryNotDue {
                run_id: record.run_id,
                retry_at: record.retry_at.unwrap_or(request.claimed_at),
                now: request.claimed_at,
            });
        }

        if !Self::can_claim(&record) {
            return if record.status.has_active_claim() {
                Err(RunStoreError::DuplicateActiveClaim {
                    task_id: record.task_id,
                    role: record.role,
                    existing_run_id: record.run_id,
                })
            } else {
                Err(RunStoreError::InvalidStatusTransition {
                    run_id: record.run_id,
                    from: record.status,
                    to: RunStatus::Claimed,
                })
            };
        }

        record.status = RunStatus::Claimed;
        record.claim_generation = record.claim_generation.next();
        record.claim_owner = Some(request.owner);
        record.session_id = request.session_id;
        record.lease_expires_at = Some(request.lease_expires_at);
        record.claimed_at = Some(request.claimed_at);
        record.updated_at = request.claimed_at;
        record.released_at = None;
        record.release_reason = None;

        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn renew_claim(
        &self,
        run_id: &RunId,
        generation: ClaimGeneration,
    ) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        let mut record = Self::load_required(&records, run_id)?;

        Self::ensure_generation(&record, generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Claimed,
            });
        }
        Self::ensure_unexpired(&record)?;

        let now = Utc::now();
        record.lease_expires_at = Some(now + ChronoDuration::seconds(DEFAULT_RENEWAL_SECONDS));
        record.updated_at = now;

        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn release_claim(&self, release: ClaimRelease) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        let mut record = Self::load_required(&records, &release.run_id)?;

        Self::ensure_generation(&record, release.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Released,
            });
        }
        Self::ensure_unexpired(&record)?;

        record.status = RunStatus::Released;
        Self::clear_claim(&mut record);
        record.released_at = Some(release.released_at);
        record.release_reason = release.reason;
        record.updated_at = release.released_at;

        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn complete_run(&self, completion: RunCompletion) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        let mut record = Self::load_required(&records, &completion.run_id)?;

        Self::ensure_generation(&record, completion.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Completed,
            });
        }
        Self::ensure_unexpired(&record)?;

        record.status = RunStatus::Completed;
        Self::clear_claim(&mut record);
        record.completed_at = Some(completion.completed_at);
        record.completion_summary = completion.summary;
        record.updated_at = completion.completed_at;

        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn record_continuation(
        &self,
        continuation: RunContinuation,
    ) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        let mut record = Self::load_required(&records, &continuation.run_id)?;

        Self::ensure_generation(&record, continuation.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: record.status,
            });
        }
        Self::ensure_unexpired(&record)?;
        if continuation.turn != record.continuation_turns.saturating_add(1) {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: record.status,
            });
        }

        record.continuation_turns = continuation.turn;
        record.last_continuation_at = Some(continuation.continued_at);
        record.last_activity_at = Some(continuation.continued_at);
        record.updated_at = continuation.continued_at;

        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn fail_run(&self, failure: RunFailure) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        let mut record = Self::load_required(&records, &failure.run_id)?;

        Self::ensure_generation(&record, failure.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Failed,
            });
        }
        Self::ensure_unexpired(&record)?;

        record.status = RunStatus::Failed;
        Self::clear_claim(&mut record);
        record.failed_at = Some(failure.failed_at);
        record.failure_reason = Some(failure.reason);
        record.updated_at = failure.failed_at;

        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn queue_retry(&self, retry: RunRetry) -> RunStoreResult<RunRecord> {
        let mut records = self.records.write();
        let mut record = Self::load_required(&records, &retry.run_id)?;

        Self::ensure_generation(&record, retry.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::RetryQueued,
            });
        }
        Self::ensure_unexpired(&record)?;

        record.status = RunStatus::RetryQueued;
        Self::clear_claim(&mut record);
        record.failed_at = Some(retry.queued_at);
        record.failure_reason = Some(retry.reason.clone());
        record.retry_queued_at = Some(retry.queued_at);
        record.retry_at = Some(retry.retry_at);
        record.retry_reason = Some(retry.reason);
        record.updated_at = retry.queued_at;

        records.insert(record.run_id.clone(), record.clone());
        Ok(record)
    }

    async fn start_retry_attempt(
        &self,
        request: RetryAttemptRequest,
    ) -> RunStoreResult<RetryAttemptStarted> {
        let mut records = self.records.write();
        let mut queued = Self::load_required(&records, &request.queued_run_id)?;

        Self::ensure_generation(&queued, request.generation)?;
        if queued.status != RunStatus::RetryQueued {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: queued.run_id,
                from: queued.status,
                to: RunStatus::Created,
            });
        }
        if let Some(retry_at) = queued.retry_at {
            if retry_at > request.created_at {
                return Err(RunStoreError::RetryNotDue {
                    run_id: queued.run_id,
                    retry_at,
                    now: request.created_at,
                });
            }
        }
        if records.contains_key(&request.new_run_id) {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: request.new_run_id,
                from: RunStatus::Created,
                to: RunStatus::Created,
            });
        }

        queued.status = RunStatus::Failed;
        queued.updated_at = request.created_at;
        queued.failed_at.get_or_insert(request.created_at);

        let mut retry_run = RunRecord::new(
            request.new_run_id,
            queued.task_id.clone(),
            queued.role.clone(),
            request.created_at,
        );
        retry_run.attempt = queued.attempt.saturating_add(1);

        if let Some(existing_run_id) = Self::active_duplicate_for(&retry_run, &records) {
            if existing_run_id != queued.run_id {
                return Err(RunStoreError::DuplicateActiveClaim {
                    task_id: retry_run.task_id,
                    role: retry_run.role,
                    existing_run_id,
                });
            }
        }

        records.insert(queued.run_id.clone(), queued.clone());
        records.insert(retry_run.run_id.clone(), retry_run.clone());
        Ok(RetryAttemptStarted {
            queued_run: queued,
            retry_run,
        })
    }

    async fn get_run(&self, run_id: &RunId) -> RunStoreResult<Option<RunRecord>> {
        Ok(self.records.read().get(run_id).cloned())
    }

    async fn active_runs(&self) -> RunStoreResult<Vec<RunRecord>> {
        let mut records: Vec<_> = self
            .records
            .read()
            .values()
            .filter(|record| record.is_active())
            .cloned()
            .collect();
        records.sort_by(|a, b| a.run_id.as_str().cmp(b.run_id.as_str()));
        Ok(records)
    }

    async fn runs_for_task(&self, task_id: &TaskId) -> RunStoreResult<Vec<RunRecord>> {
        let mut records: Vec<_> = self
            .records
            .read()
            .values()
            .filter(|record| &record.task_id == task_id)
            .cloned()
            .collect();
        records.sort_by(|a, b| a.run_id.as_str().cmp(b.run_id.as_str()));
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_store_contract;

    #[tokio::test]
    async fn in_memory_run_store_contract_create_active_run() {
        run_store_contract::create_active_run(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_reject_duplicate_claim() {
        run_store_contract::reject_duplicate_claim(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_reject_duplicate_active_run() {
        run_store_contract::reject_duplicate_active_run(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_renew_active_claim() {
        run_store_contract::renew_active_claim(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_reject_expired_claim_renewal() {
        run_store_contract::reject_expired_claim_renewal(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_release_and_requeue() {
        run_store_contract::release_and_requeue(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_reclaim_increments_generation() {
        run_store_contract::reclaim_increments_generation(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_stale_completion_rejected() {
        run_store_contract::stale_completion_rejected(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_continuation_records_same_run_progress() {
        run_store_contract::continuation_records_same_run_progress(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_active_run_query_excludes_terminal_runs() {
        run_store_contract::active_run_query_excludes_terminal_runs(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_retry_queued_blocks_claim_until_due() {
        run_store_contract::retry_queued_blocks_claim_until_due(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_retry_attempt_creates_new_run() {
        run_store_contract::retry_attempt_creates_new_run(&InMemoryRunStore::new()).await;
    }

    #[tokio::test]
    async fn in_memory_run_store_contract_duplicate_retry_queue_rejected() {
        run_store_contract::duplicate_retry_queue_rejected(&InMemoryRunStore::new()).await;
    }
}
