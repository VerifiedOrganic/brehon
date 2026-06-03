//! Durable proof bundle projection port.

use async_trait::async_trait;
use thiserror::Error;

use brehon_types::{Event, EventId, ProofBundle, ProofBundleId, RunId, TaskId};

/// Result type for proof store operations.
pub type ProofStoreResult<T> = Result<T, ProofStoreError>;

/// Errors returned by durable proof store implementations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProofStoreError {
    /// Storage backend error.
    #[error("proof store storage error: {0}")]
    Storage(String),

    /// Serialization error.
    #[error("proof store serialization error: {0}")]
    Serialization(String),
}

/// Durable proof bundle projection.
#[async_trait]
pub trait ProofStore: Send + Sync {
    /// Rebuild proof bundle projections from the durable proof event stream.
    async fn rebuild_proof_projection(&self) -> ProofStoreResult<usize>;

    /// Apply one event to the proof projection.
    async fn apply_proof_event(
        &self,
        event: &Event,
        event_id: EventId,
    ) -> ProofStoreResult<Option<ProofBundle>>;

    /// Get a proof bundle by id.
    async fn proof_bundle(
        &self,
        proof_bundle_id: &ProofBundleId,
    ) -> ProofStoreResult<Option<ProofBundle>>;

    /// Get the current proof bundle for a task.
    async fn proof_bundle_for_task(
        &self,
        task_id: &TaskId,
    ) -> ProofStoreResult<Option<ProofBundle>>;

    /// Get the current proof bundle for a run.
    async fn proof_bundle_for_run(&self, run_id: &RunId) -> ProofStoreResult<Option<ProofBundle>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_object_safe(_: &dyn ProofStore) {}

    struct NoopProofStore;

    #[async_trait]
    impl ProofStore for NoopProofStore {
        async fn rebuild_proof_projection(&self) -> ProofStoreResult<usize> {
            Ok(0)
        }

        async fn apply_proof_event(
            &self,
            _event: &Event,
            _event_id: EventId,
        ) -> ProofStoreResult<Option<ProofBundle>> {
            Ok(None)
        }

        async fn proof_bundle(
            &self,
            _proof_bundle_id: &ProofBundleId,
        ) -> ProofStoreResult<Option<ProofBundle>> {
            Ok(None)
        }

        async fn proof_bundle_for_task(
            &self,
            _task_id: &TaskId,
        ) -> ProofStoreResult<Option<ProofBundle>> {
            Ok(None)
        }

        async fn proof_bundle_for_run(
            &self,
            _run_id: &RunId,
        ) -> ProofStoreResult<Option<ProofBundle>> {
            Ok(None)
        }
    }

    #[test]
    fn proof_store_trait_is_object_safe() {
        let store = NoopProofStore;
        assert_object_safe(&store);
    }

    #[test]
    fn proof_store_errors_are_typed() {
        let err = ProofStoreError::Serialization("bad json".into());
        assert!(matches!(err, ProofStoreError::Serialization(_)));
    }
}
