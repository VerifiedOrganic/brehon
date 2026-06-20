//! Fjall-backed durable run store.

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use fjall::{Keyspace, PartitionHandle, PersistMode};
use parking_lot::Mutex;
use std::sync::Arc;

use brehon_ports::{
    ClaimRelease, ClaimRequest, RetryAttemptRequest, RetryAttemptStarted, RunCompletion,
    RunContinuation, RunFailure, RunRetry, RunStore, RunStoreError, RunStoreResult,
};
use brehon_types::{ClaimGeneration, RunId, RunRecord, RunStatus, TaskId};

use crate::keys::{
    run_active_role_index_key, run_active_task_role_key, run_owner_index_key, run_record_key,
    run_session_index_key, run_task_index_key, run_task_index_prefix,
};
use crate::store::FjallEventStore;

const DEFAULT_RENEWAL_SECONDS: i64 = 60;

/// Manages the dedicated fjall run partition and indexes.
pub struct RunStoreManager {
    keyspace: Keyspace,
    runs: PartitionHandle,
    mutation_lock: Arc<Mutex<()>>,
}

impl RunStoreManager {
    /// Create a run store manager.
    pub fn new(keyspace: Keyspace, runs: PartitionHandle) -> Self {
        Self {
            keyspace,
            runs,
            mutation_lock: Arc::new(Mutex::new(())),
        }
    }

    fn serialize(record: &RunRecord) -> RunStoreResult<Vec<u8>> {
        serde_json::to_vec(record).map_err(|err| RunStoreError::Serialization(err.to_string()))
    }

    fn deserialize(bytes: &[u8]) -> RunStoreResult<RunRecord> {
        serde_json::from_slice(bytes).map_err(|err| RunStoreError::Serialization(err.to_string()))
    }

    fn get_run_locked(&self, run_id: &RunId) -> RunStoreResult<Option<RunRecord>> {
        let key = run_record_key(run_id.as_str());
        let value = self
            .runs
            .get(&key)
            .map_err(|err| RunStoreError::Storage(err.to_string()))?;
        value.as_deref().map(Self::deserialize).transpose()
    }

    fn active_task_role_marker(&self, record: &RunRecord) -> Vec<u8> {
        run_active_task_role_key(record.task_id.as_str(), record.role.as_str())
    }

    fn record_index_keys(record: &RunRecord) -> Vec<Vec<u8>> {
        let mut keys = vec![run_task_index_key(
            record.task_id.as_str(),
            record.run_id.as_str(),
        )];

        if let Some(session_id) = record.session_id.as_ref() {
            keys.push(run_session_index_key(
                session_id.as_str(),
                record.run_id.as_str(),
            ));
        }

        if let Some(owner) = record.claim_owner.as_ref() {
            keys.push(run_owner_index_key(owner.as_str(), record.run_id.as_str()));
        }

        if record.is_active() {
            keys.push(run_active_role_index_key(
                record.role.as_str(),
                record.run_id.as_str(),
            ));
            keys.push(run_active_task_role_key(
                record.task_id.as_str(),
                record.role.as_str(),
            ));
        }

        keys
    }

    fn remove_record_indexes(&self, batch: &mut fjall::Batch, record: &RunRecord) {
        for key in Self::record_index_keys(record) {
            batch.remove(&self.runs, key);
        }
    }

    fn insert_record_indexes(&self, batch: &mut fjall::Batch, record: &RunRecord) {
        for key in Self::record_index_keys(record) {
            if key == self.active_task_role_marker(record) {
                batch.insert(&self.runs, key, record.run_id.as_str().as_bytes());
            } else {
                batch.insert(&self.runs, key, b"");
            }
        }
    }

    fn commit_record(
        &self,
        previous: Option<&RunRecord>,
        record: &RunRecord,
    ) -> RunStoreResult<()> {
        let mut batch = self.keyspace.batch().durability(Some(PersistMode::SyncAll));

        if let Some(previous) = previous {
            self.remove_record_indexes(&mut batch, previous);
        }

        batch.insert(
            &self.runs,
            run_record_key(record.run_id.as_str()),
            Self::serialize(record)?,
        );
        self.insert_record_indexes(&mut batch, record);

        batch
            .commit()
            .map_err(|err| RunStoreError::Storage(err.to_string()))
    }

    fn find_active_duplicate(&self, record: &RunRecord) -> RunStoreResult<Option<RunId>> {
        let marker = self.active_task_role_marker(record);
        let Some(value) = self
            .runs
            .get(&marker)
            .map_err(|err| RunStoreError::Storage(err.to_string()))?
        else {
            return Ok(None);
        };

        let existing_id = RunId::new(String::from_utf8_lossy(value.as_ref()).to_string());
        if existing_id == record.run_id {
            return Ok(None);
        }

        let Some(existing) = self.get_run_locked(&existing_id)? else {
            self.runs
                .remove(&marker)
                .map_err(|err| RunStoreError::Storage(err.to_string()))?;
            return Ok(None);
        };

        if existing.is_active() {
            Ok(Some(existing_id))
        } else {
            self.runs
                .remove(&marker)
                .map_err(|err| RunStoreError::Storage(err.to_string()))?;
            Ok(None)
        }
    }

    fn load_required(&self, run_id: &RunId) -> RunStoreResult<RunRecord> {
        self.get_run_locked(run_id)?
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

    /// Create a run record.
    pub fn create_run(&self, mut record: RunRecord) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();

        if self.get_run_locked(&record.run_id)?.is_some() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: record.status,
            });
        }

        if record.is_active() {
            if let Some(existing_run_id) = self.find_active_duplicate(&record)? {
                return Err(RunStoreError::DuplicateActiveClaim {
                    task_id: record.task_id,
                    role: record.role,
                    existing_run_id,
                });
            }
        }

        record.updated_at = record.created_at;
        self.commit_record(None, &record)?;
        Ok(record)
    }

    /// Claim a run record.
    pub fn claim_run(&self, request: ClaimRequest) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();
        let mut record = self.load_required(&request.run_id)?;

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

        let previous = record.clone();
        record.status = RunStatus::Claimed;
        record.claim_generation = record.claim_generation.next();
        record.claim_owner = Some(request.owner);
        record.session_id = request.session_id;
        record.lease_expires_at = Some(request.lease_expires_at);
        record.claimed_at = Some(request.claimed_at);
        record.updated_at = request.claimed_at;
        record.released_at = None;
        record.release_reason = None;

        self.commit_record(Some(&previous), &record)?;
        Ok(record)
    }

    /// Renew a current claim.
    pub fn renew_claim(
        &self,
        run_id: &RunId,
        generation: ClaimGeneration,
    ) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();
        let mut record = self.load_required(run_id)?;

        Self::ensure_generation(&record, generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Claimed,
            });
        }
        Self::ensure_unexpired(&record)?;

        let previous = record.clone();
        let now = Utc::now();
        record.lease_expires_at = Some(now + ChronoDuration::seconds(DEFAULT_RENEWAL_SECONDS));
        record.updated_at = now;

        self.commit_record(Some(&previous), &record)?;
        Ok(record)
    }

    /// Release a current claim.
    pub fn release_claim(&self, release: ClaimRelease) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();
        let mut record = self.load_required(&release.run_id)?;

        Self::ensure_generation(&record, release.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Released,
            });
        }
        Self::ensure_unexpired(&record)?;

        let previous = record.clone();
        record.status = RunStatus::Released;
        Self::clear_claim(&mut record);
        record.released_at = Some(release.released_at);
        record.release_reason = release.reason;
        record.updated_at = release.released_at;

        self.commit_record(Some(&previous), &record)?;
        Ok(record)
    }

    /// Complete a current run.
    pub fn complete_run(&self, completion: RunCompletion) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();
        let mut record = self.load_required(&completion.run_id)?;

        Self::ensure_generation(&record, completion.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Completed,
            });
        }
        Self::ensure_unexpired(&record)?;

        let previous = record.clone();
        record.status = RunStatus::Completed;
        Self::clear_claim(&mut record);
        record.completed_at = Some(completion.completed_at);
        record.completion_summary = completion.summary;
        record.updated_at = completion.completed_at;

        self.commit_record(Some(&previous), &record)?;
        Ok(record)
    }

    /// Fail a current run.
    pub fn fail_run(&self, failure: RunFailure) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();
        let mut record = self.load_required(&failure.run_id)?;

        Self::ensure_generation(&record, failure.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::Failed,
            });
        }
        Self::ensure_unexpired(&record)?;

        let previous = record.clone();
        record.status = RunStatus::Failed;
        Self::clear_claim(&mut record);
        record.failed_at = Some(failure.failed_at);
        record.failure_reason = Some(failure.reason);
        record.updated_at = failure.failed_at;

        self.commit_record(Some(&previous), &record)?;
        Ok(record)
    }

    /// Record a continuation prompt on a current run.
    pub fn record_continuation(&self, continuation: RunContinuation) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();
        let mut record = self.load_required(&continuation.run_id)?;

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

        let previous = record.clone();
        record.continuation_turns = continuation.turn;
        record.last_continuation_at = Some(continuation.continued_at);
        record.last_activity_at = Some(continuation.continued_at);
        record.updated_at = continuation.continued_at;

        self.commit_record(Some(&previous), &record)?;
        Ok(record)
    }

    /// Queue a current run for retry.
    pub fn queue_retry(&self, retry: RunRetry) -> RunStoreResult<RunRecord> {
        let _guard = self.mutation_lock.lock();
        let mut record = self.load_required(&retry.run_id)?;

        Self::ensure_generation(&record, retry.generation)?;
        if !record.status.has_active_claim() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: record.run_id,
                from: record.status,
                to: RunStatus::RetryQueued,
            });
        }
        Self::ensure_unexpired(&record)?;

        let previous = record.clone();
        record.status = RunStatus::RetryQueued;
        Self::clear_claim(&mut record);
        record.failed_at = Some(retry.queued_at);
        record.failure_reason = Some(retry.reason.clone());
        record.retry_queued_at = Some(retry.queued_at);
        record.retry_at = Some(retry.retry_at);
        record.retry_reason = Some(retry.reason);
        record.updated_at = retry.queued_at;

        self.commit_record(Some(&previous), &record)?;
        Ok(record)
    }

    /// Close a due retry-queued run and create the next attempt.
    pub fn start_retry_attempt(
        &self,
        request: RetryAttemptRequest,
    ) -> RunStoreResult<RetryAttemptStarted> {
        let _guard = self.mutation_lock.lock();
        let mut queued = self.load_required(&request.queued_run_id)?;

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
        if self.get_run_locked(&request.new_run_id)?.is_some() {
            return Err(RunStoreError::InvalidStatusTransition {
                run_id: request.new_run_id,
                from: RunStatus::Created,
                to: RunStatus::Created,
            });
        }

        let previous = queued.clone();
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

        if let Some(existing_run_id) = self.find_active_duplicate(&retry_run)? {
            if existing_run_id != queued.run_id {
                return Err(RunStoreError::DuplicateActiveClaim {
                    task_id: retry_run.task_id,
                    role: retry_run.role,
                    existing_run_id,
                });
            }
        }

        let mut batch = self.keyspace.batch().durability(Some(PersistMode::SyncAll));
        self.remove_record_indexes(&mut batch, &previous);
        batch.insert(
            &self.runs,
            run_record_key(queued.run_id.as_str()),
            Self::serialize(&queued)?,
        );
        self.insert_record_indexes(&mut batch, &queued);
        batch.insert(
            &self.runs,
            run_record_key(retry_run.run_id.as_str()),
            Self::serialize(&retry_run)?,
        );
        self.insert_record_indexes(&mut batch, &retry_run);
        batch
            .commit()
            .map_err(|err| RunStoreError::Storage(err.to_string()))?;

        Ok(RetryAttemptStarted {
            queued_run: queued,
            retry_run,
        })
    }

    /// Get a run by id.
    pub fn get_run(&self, run_id: &RunId) -> RunStoreResult<Option<RunRecord>> {
        let _guard = self.mutation_lock.lock();
        self.get_run_locked(run_id)
    }

    /// Return all non-terminal runs.
    pub fn active_runs(&self) -> RunStoreResult<Vec<RunRecord>> {
        let _guard = self.mutation_lock.lock();
        let mut records = Vec::new();

        for item in self.runs.prefix(b"index:run:active-role:") {
            let (key, _value) = item.map_err(|err| RunStoreError::Storage(err.to_string()))?;
            let key = String::from_utf8_lossy(&key);
            let Some(run_id) = key.rsplit(':').next() else {
                continue;
            };
            if let Some(record) = self.get_run_locked(&RunId::new(run_id.to_string()))? {
                if record.is_active() {
                    records.push(record);
                }
            }
        }

        records.sort_by(|a, b| a.run_id.as_str().cmp(b.run_id.as_str()));
        Ok(records)
    }

    /// Return all runs for a task.
    pub fn runs_for_task(&self, task_id: &TaskId) -> RunStoreResult<Vec<RunRecord>> {
        let _guard = self.mutation_lock.lock();
        let mut records = Vec::new();

        for item in self.runs.prefix(&run_task_index_prefix(task_id.as_str())) {
            let (key, _value) = item.map_err(|err| RunStoreError::Storage(err.to_string()))?;
            let key = String::from_utf8_lossy(&key);
            let Some(run_id) = key.rsplit(':').next() else {
                continue;
            };
            if let Some(record) = self.get_run_locked(&RunId::new(run_id.to_string()))? {
                records.push(record);
            }
        }

        records.sort_by(|a, b| a.run_id.as_str().cmp(b.run_id.as_str()));
        Ok(records)
    }
}

/// Map the outcome of a `spawn_blocking` run-store mutation into a
/// `RunStoreResult`. The synchronous fjall fsync (`PersistMode::SyncAll`) runs on
/// the blocking pool so it never parks a Tokio worker; a panic inside the closure
/// (e.g. an unexpected fjall failure) surfaces here as a `JoinError`, which we fail
/// closed into a `RunStoreError::Storage` rather than propagating the panic. This
/// relies on the `panic = "unwind"` profile setting documented in the workspace
/// `Cargo.toml`.
fn map_run_store_blocking<T>(
    joined: Result<RunStoreResult<T>, tokio::task::JoinError>,
) -> RunStoreResult<T> {
    match joined {
        Ok(inner) => inner,
        Err(join_err) => Err(RunStoreError::Storage(format!(
            "run store storage task panicked: {join_err}"
        ))),
    }
}

#[async_trait]
impl RunStore for FjallEventStore {
    async fn create_run(&self, record: RunRecord) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().create_run(record)).await,
        )
    }

    async fn claim_run(&self, request: ClaimRequest) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().claim_run(request)).await,
        )
    }

    async fn renew_claim(
        &self,
        run_id: &RunId,
        generation: ClaimGeneration,
    ) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        let run_id = run_id.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().renew_claim(&run_id, generation))
                .await,
        )
    }

    async fn release_claim(&self, release: ClaimRelease) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().release_claim(release)).await,
        )
    }

    async fn complete_run(&self, completion: RunCompletion) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().complete_run(completion)).await,
        )
    }

    async fn record_continuation(
        &self,
        continuation: RunContinuation,
    ) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || {
                store.run_store().record_continuation(continuation)
            })
            .await,
        )
    }

    async fn fail_run(&self, failure: RunFailure) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().fail_run(failure)).await,
        )
    }

    async fn queue_retry(&self, retry: RunRetry) -> RunStoreResult<RunRecord> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().queue_retry(retry)).await,
        )
    }

    async fn start_retry_attempt(
        &self,
        request: RetryAttemptRequest,
    ) -> RunStoreResult<RetryAttemptStarted> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().start_retry_attempt(request))
                .await,
        )
    }

    async fn get_run(&self, run_id: &RunId) -> RunStoreResult<Option<RunRecord>> {
        let store = self.clone();
        let run_id = run_id.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().get_run(&run_id)).await,
        )
    }

    async fn active_runs(&self) -> RunStoreResult<Vec<RunRecord>> {
        let store = self.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().active_runs()).await,
        )
    }

    async fn runs_for_task(&self, task_id: &TaskId) -> RunStoreResult<Vec<RunRecord>> {
        let store = self.clone();
        let task_id = task_id.clone();
        map_run_store_blocking(
            tokio::task::spawn_blocking(move || store.run_store().runs_for_task(&task_id)).await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::run_store_contract;
    use tempfile::tempdir;

    fn store() -> FjallEventStore {
        let dir = tempdir().unwrap();
        FjallEventStore::new(dir.path()).unwrap()
    }

    #[tokio::test]
    async fn fjall_run_store_contract_create_active_run() {
        run_store_contract::create_active_run(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_reject_duplicate_claim() {
        run_store_contract::reject_duplicate_claim(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_reject_duplicate_active_run() {
        run_store_contract::reject_duplicate_active_run(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_renew_active_claim() {
        run_store_contract::renew_active_claim(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_reject_expired_claim_renewal() {
        run_store_contract::reject_expired_claim_renewal(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_release_and_requeue() {
        run_store_contract::release_and_requeue(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_reclaim_increments_generation() {
        run_store_contract::reclaim_increments_generation(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_stale_completion_rejected() {
        run_store_contract::stale_completion_rejected(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_stale_generation_completion_rejected() {
        run_store_contract::stale_completion_rejected(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_continuation_records_same_run_progress() {
        run_store_contract::continuation_records_same_run_progress(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_active_run_query_excludes_terminal_runs() {
        run_store_contract::active_run_query_excludes_terminal_runs(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_retry_queued_blocks_claim_until_due() {
        run_store_contract::retry_queued_blocks_claim_until_due(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_retry_attempt_creates_new_run() {
        run_store_contract::retry_attempt_creates_new_run(&store()).await;
    }

    #[tokio::test]
    async fn fjall_run_store_contract_duplicate_retry_queue_rejected() {
        run_store_contract::duplicate_retry_queue_rejected(&store()).await;
    }
}
