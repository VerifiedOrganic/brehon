//! Durable run store port.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use brehon_types::{
    ClaimGeneration, ClaimOwner, RunId, RunRecord, RunRole, RunStatus, SessionId, TaskId,
};

/// Result type for run store operations.
pub type RunStoreResult<T> = Result<T, RunStoreError>;

/// Errors returned by durable run store implementations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RunStoreError {
    /// Another non-terminal run already exists for this task/role lane.
    #[error("duplicate active run for task {task_id} role {role}: existing run {existing_run_id}")]
    DuplicateActiveClaim {
        /// Task with an existing active run.
        task_id: TaskId,
        /// Role lane with an existing active run.
        role: RunRole,
        /// Existing active run id.
        existing_run_id: RunId,
    },

    /// Mutation used a stale claim generation.
    #[error("stale generation for run {run_id}: expected {expected}, got {actual}")]
    StaleGeneration {
        /// Run being mutated.
        run_id: RunId,
        /// Expected/current generation.
        expected: ClaimGeneration,
        /// Generation supplied by caller.
        actual: ClaimGeneration,
    },

    /// Operation is invalid for the current status.
    #[error("invalid run status transition for {run_id}: {from} -> {to}")]
    InvalidStatusTransition {
        /// Run being mutated.
        run_id: RunId,
        /// Current status.
        from: RunStatus,
        /// Requested status.
        to: RunStatus,
    },

    /// Claim lease has expired.
    #[error("claim lease expired for run {run_id} generation {generation}")]
    LeaseExpired {
        /// Run being mutated.
        run_id: RunId,
        /// Expired generation.
        generation: ClaimGeneration,
    },

    /// Retry-queued run is not due yet.
    #[error("retry for run {run_id} is not due until {retry_at}; checked at {now}")]
    RetryNotDue {
        /// Retry-queued run.
        run_id: RunId,
        /// Earliest retry time.
        retry_at: DateTime<Utc>,
        /// Time used by the caller.
        now: DateTime<Utc>,
    },

    /// Run was not found.
    #[error("run not found: {run_id}")]
    NotFound {
        /// Missing run id.
        run_id: RunId,
    },

    /// Storage backend error.
    #[error("run store storage error: {0}")]
    Storage(String),

    /// Serialization error.
    #[error("run store serialization error: {0}")]
    Serialization(String),
}

/// Claim request for a durable run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRequest {
    /// Run to claim.
    pub run_id: RunId,
    /// Claim owner.
    pub owner: ClaimOwner,
    /// Optional live session associated with the claim.
    pub session_id: Option<SessionId>,
    /// When the claim is made.
    pub claimed_at: DateTime<Utc>,
    /// Absolute claim lease expiration.
    pub lease_expires_at: DateTime<Utc>,
}

impl ClaimRequest {
    /// Create a claim request.
    pub fn new(
        run_id: RunId,
        owner: ClaimOwner,
        session_id: Option<SessionId>,
        claimed_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            run_id,
            owner,
            session_id,
            claimed_at,
            lease_expires_at,
        }
    }
}

/// Release request for a durable run claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRelease {
    /// Run to release.
    pub run_id: RunId,
    /// Generation being released.
    pub generation: ClaimGeneration,
    /// Release time.
    pub released_at: DateTime<Utc>,
    /// Optional release reason.
    pub reason: Option<String>,
}

impl ClaimRelease {
    /// Create a claim release request.
    pub fn new(
        run_id: RunId,
        generation: ClaimGeneration,
        released_at: DateTime<Utc>,
        reason: Option<String>,
    ) -> Self {
        Self {
            run_id,
            generation,
            released_at,
            reason,
        }
    }
}

/// Completion request for a durable run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCompletion {
    /// Run to complete.
    pub run_id: RunId,
    /// Generation completing the run.
    pub generation: ClaimGeneration,
    /// Completion time.
    pub completed_at: DateTime<Utc>,
    /// Optional completion summary.
    pub summary: Option<String>,
}

impl RunCompletion {
    /// Create a run completion request.
    pub fn new(
        run_id: RunId,
        generation: ClaimGeneration,
        completed_at: DateTime<Utc>,
        summary: Option<String>,
    ) -> Self {
        Self {
            run_id,
            generation,
            completed_at,
            summary,
        }
    }
}

/// Continuation prompt record for an active durable run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContinuation {
    /// Run that received the continuation prompt.
    pub run_id: RunId,
    /// Generation receiving the continuation prompt.
    pub generation: ClaimGeneration,
    /// Continuation prompt timestamp.
    pub continued_at: DateTime<Utc>,
    /// New continuation turn count.
    pub turn: u32,
}

impl RunContinuation {
    /// Create a run continuation request.
    pub fn new(
        run_id: RunId,
        generation: ClaimGeneration,
        continued_at: DateTime<Utc>,
        turn: u32,
    ) -> Self {
        Self {
            run_id,
            generation,
            continued_at,
            turn,
        }
    }
}

/// Failure request for a durable run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFailure {
    /// Run to fail.
    pub run_id: RunId,
    /// Generation failing the run.
    pub generation: ClaimGeneration,
    /// Failure time.
    pub failed_at: DateTime<Utc>,
    /// Failure reason.
    pub reason: String,
}

/// Retry queue request for a durable run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRetry {
    /// Run to queue for retry.
    pub run_id: RunId,
    /// Generation queuing the retry.
    pub generation: ClaimGeneration,
    /// Queue time.
    pub queued_at: DateTime<Utc>,
    /// Earliest time the next attempt may be created.
    pub retry_at: DateTime<Utc>,
    /// Retry reason.
    pub reason: String,
}

impl RunRetry {
    /// Create a run retry request.
    pub fn new(
        run_id: RunId,
        generation: ClaimGeneration,
        queued_at: DateTime<Utc>,
        retry_at: DateTime<Utc>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            generation,
            queued_at,
            retry_at,
            reason: reason.into(),
        }
    }
}

/// Request to create the next attempt from a retry-queued run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAttemptRequest {
    /// Retry-queued run to close out.
    pub queued_run_id: RunId,
    /// Generation of the retry-queued run.
    pub generation: ClaimGeneration,
    /// New run id for the next attempt.
    pub new_run_id: RunId,
    /// Creation time for the next attempt.
    pub created_at: DateTime<Utc>,
}

impl RetryAttemptRequest {
    /// Create a retry attempt request.
    pub fn new(
        queued_run_id: RunId,
        generation: ClaimGeneration,
        new_run_id: RunId,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            queued_run_id,
            generation,
            new_run_id,
            created_at,
        }
    }
}

/// Result of creating the next retry attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAttemptStarted {
    /// Previous retry-queued run, now terminal.
    pub queued_run: RunRecord,
    /// New active run created for the next attempt.
    pub retry_run: RunRecord,
}

impl RunFailure {
    /// Create a run failure request.
    pub fn new(
        run_id: RunId,
        generation: ClaimGeneration,
        failed_at: DateTime<Utc>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            generation,
            failed_at,
            reason: reason.into(),
        }
    }
}

/// Durable run store port.
#[async_trait]
pub trait RunStore: Send + Sync {
    /// Create a durable run record.
    async fn create_run(&self, record: RunRecord) -> RunStoreResult<RunRecord>;

    /// Claim a durable run.
    async fn claim_run(&self, request: ClaimRequest) -> RunStoreResult<RunRecord>;

    /// Renew a claim if the caller holds the current generation.
    async fn renew_claim(
        &self,
        run_id: &RunId,
        generation: ClaimGeneration,
    ) -> RunStoreResult<RunRecord>;

    /// Release a claim and make the run claimable again.
    async fn release_claim(&self, release: ClaimRelease) -> RunStoreResult<RunRecord>;

    /// Complete a run.
    async fn complete_run(&self, completion: RunCompletion) -> RunStoreResult<RunRecord>;

    /// Record a continuation prompt on an active run.
    async fn record_continuation(&self, continuation: RunContinuation)
        -> RunStoreResult<RunRecord>;

    /// Fail a run.
    async fn fail_run(&self, failure: RunFailure) -> RunStoreResult<RunRecord>;

    /// Queue a failed run for a bounded retry.
    async fn queue_retry(&self, retry: RunRetry) -> RunStoreResult<RunRecord>;

    /// Close a due retry-queued run and create the next attempt.
    async fn start_retry_attempt(
        &self,
        request: RetryAttemptRequest,
    ) -> RunStoreResult<RetryAttemptStarted>;

    /// Get a run by id.
    async fn get_run(&self, run_id: &RunId) -> RunStoreResult<Option<RunRecord>>;

    /// Return all non-terminal runs.
    async fn active_runs(&self) -> RunStoreResult<Vec<RunRecord>>;

    /// Return all runs for a task.
    async fn runs_for_task(&self, task_id: &TaskId) -> RunStoreResult<Vec<RunRecord>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_object_safe(_: &dyn RunStore) {}

    struct NoopRunStore;

    #[async_trait]
    impl RunStore for NoopRunStore {
        async fn create_run(&self, record: RunRecord) -> RunStoreResult<RunRecord> {
            Ok(record)
        }

        async fn claim_run(&self, _request: ClaimRequest) -> RunStoreResult<RunRecord> {
            Err(RunStoreError::Storage("not implemented".into()))
        }

        async fn renew_claim(
            &self,
            run_id: &RunId,
            _generation: ClaimGeneration,
        ) -> RunStoreResult<RunRecord> {
            Err(RunStoreError::NotFound {
                run_id: run_id.clone(),
            })
        }

        async fn release_claim(&self, _release: ClaimRelease) -> RunStoreResult<RunRecord> {
            Err(RunStoreError::Storage("not implemented".into()))
        }

        async fn complete_run(&self, _completion: RunCompletion) -> RunStoreResult<RunRecord> {
            Err(RunStoreError::Storage("not implemented".into()))
        }

        async fn record_continuation(
            &self,
            _continuation: RunContinuation,
        ) -> RunStoreResult<RunRecord> {
            Err(RunStoreError::Storage("not implemented".into()))
        }

        async fn fail_run(&self, _failure: RunFailure) -> RunStoreResult<RunRecord> {
            Err(RunStoreError::Storage("not implemented".into()))
        }

        async fn queue_retry(&self, _retry: RunRetry) -> RunStoreResult<RunRecord> {
            Err(RunStoreError::Storage("not implemented".into()))
        }

        async fn start_retry_attempt(
            &self,
            _request: RetryAttemptRequest,
        ) -> RunStoreResult<RetryAttemptStarted> {
            Err(RunStoreError::Storage("not implemented".into()))
        }

        async fn get_run(&self, _run_id: &RunId) -> RunStoreResult<Option<RunRecord>> {
            Ok(None)
        }

        async fn active_runs(&self) -> RunStoreResult<Vec<RunRecord>> {
            Ok(Vec::new())
        }

        async fn runs_for_task(&self, _task_id: &TaskId) -> RunStoreResult<Vec<RunRecord>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn run_store_trait_is_object_safe() {
        let store = NoopRunStore;
        assert_object_safe(&store);
    }

    #[test]
    fn run_store_errors_are_typed() {
        let err = RunStoreError::StaleGeneration {
            run_id: RunId::new("run-1"),
            expected: ClaimGeneration::new(2),
            actual: ClaimGeneration::new(1),
        };
        assert!(matches!(err, RunStoreError::StaleGeneration { .. }));
    }
}
