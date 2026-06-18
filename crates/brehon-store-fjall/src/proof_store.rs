//! Fjall-backed proof bundle projection.

use async_trait::async_trait;
use fjall::{Keyspace, PartitionHandle, PersistMode};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

use brehon_ports::{ProofStore, ProofStoreError, ProofStoreResult};
use brehon_types::{
    deserialize_event_envelope, Event, EventId, EventKind, ProofBlocker, ProofBundle,
    ProofBundleId, ProofCheck, ProofCommand, ProofDecision, ProofIntegration, ProofReview, RunId,
    TaskId,
};

use crate::keys::{proof_bundle_key, proof_run_index_key, proof_task_index_key};
use crate::store::FjallEventStore;

/// Manages the dedicated fjall proof projection partition and indexes.
pub struct ProofStoreManager {
    keyspace: Keyspace,
    proofs: PartitionHandle,
    mutation_lock: Arc<Mutex<()>>,
}

impl ProofStoreManager {
    /// Create a proof store manager.
    pub fn new(keyspace: Keyspace, proofs: PartitionHandle) -> Self {
        Self {
            keyspace,
            proofs,
            mutation_lock: Arc::new(Mutex::new(())),
        }
    }

    fn serialize(bundle: &ProofBundle) -> ProofStoreResult<Vec<u8>> {
        serde_json::to_vec(bundle).map_err(|err| ProofStoreError::Serialization(err.to_string()))
    }

    fn deserialize(bytes: &[u8]) -> ProofStoreResult<ProofBundle> {
        serde_json::from_slice(bytes).map_err(|err| ProofStoreError::Serialization(err.to_string()))
    }

    fn storage_error(err: impl std::fmt::Display) -> ProofStoreError {
        ProofStoreError::Storage(err.to_string())
    }

    fn get_bundle_locked(
        &self,
        proof_bundle_id: &ProofBundleId,
    ) -> ProofStoreResult<Option<ProofBundle>> {
        let value = self
            .proofs
            .get(proof_bundle_key(proof_bundle_id.as_str()))
            .map_err(Self::storage_error)?;
        value.as_deref().map(Self::deserialize).transpose()
    }

    fn bundle_id_from_index(&self, key: Vec<u8>) -> ProofStoreResult<Option<ProofBundleId>> {
        let Some(value) = self.proofs.get(key).map_err(Self::storage_error)? else {
            return Ok(None);
        };
        Ok(Some(ProofBundleId::new(
            String::from_utf8_lossy(value.as_ref()).to_string(),
        )))
    }

    fn indexed_bundle(&self, key: Vec<u8>) -> ProofStoreResult<Option<ProofBundle>> {
        let Some(proof_bundle_id) = self.bundle_id_from_index(key)? else {
            return Ok(None);
        };
        self.get_bundle_locked(&proof_bundle_id)
    }

    fn index_keys(bundle: &ProofBundle) -> Vec<Vec<u8>> {
        let mut keys = vec![proof_task_index_key(bundle.task_id.as_str())];
        keys.extend(
            bundle
                .run_ids
                .iter()
                .map(|run_id| proof_run_index_key(run_id.as_str())),
        );
        keys
    }

    fn remove_indexes(&self, batch: &mut fjall::Batch, bundle: &ProofBundle) {
        for key in Self::index_keys(bundle) {
            batch.remove(&self.proofs, key);
        }
    }

    fn insert_bundle(
        &self,
        batch: &mut fjall::Batch,
        bundle: &ProofBundle,
    ) -> ProofStoreResult<()> {
        let proof_bundle_id = bundle.proof_bundle_id.as_str().as_bytes();
        batch.insert(
            &self.proofs,
            proof_bundle_key(bundle.proof_bundle_id.as_str()),
            Self::serialize(bundle)?,
        );
        batch.insert(
            &self.proofs,
            proof_task_index_key(bundle.task_id.as_str()),
            proof_bundle_id,
        );
        for run_id in &bundle.run_ids {
            batch.insert(
                &self.proofs,
                proof_run_index_key(run_id.as_str()),
                proof_bundle_id,
            );
        }
        Ok(())
    }

    fn commit_bundle(
        &self,
        previous: Option<&ProofBundle>,
        bundle: &ProofBundle,
    ) -> ProofStoreResult<()> {
        let mut batch = self.keyspace.batch().durability(Some(PersistMode::SyncAll));
        if let Some(previous) = previous {
            self.remove_indexes(&mut batch, previous);
        }
        self.insert_bundle(&mut batch, bundle)?;
        batch.commit().map_err(Self::storage_error)
    }

    fn clear_and_insert_bundles(&self, bundles: &[ProofBundle]) -> ProofStoreResult<()> {
        let mut keys = Vec::new();
        for item in self.proofs.prefix(b"") {
            let (key, _) = item.map_err(Self::storage_error)?;
            keys.push(key.to_vec());
        }

        let mut batch = self.keyspace.batch().durability(Some(PersistMode::SyncAll));
        for key in keys {
            batch.remove(&self.proofs, key);
        }
        for bundle in bundles {
            self.insert_bundle(&mut batch, bundle)?;
        }
        batch.commit().map_err(Self::storage_error)
    }

    fn base_bundle(proof_bundle_id: ProofBundleId, task_id: TaskId, event: &Event) -> ProofBundle {
        ProofBundle::empty(proof_bundle_id, task_id, event.timestamp)
    }

    fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
        if !items.contains(&item) {
            items.push(item);
        }
    }

    fn merge_run_ids(bundle: &mut ProofBundle, run_ids: &[RunId]) {
        for run_id in run_ids {
            Self::push_unique(&mut bundle.run_ids, run_id.clone());
        }
    }

    fn record_command(bundle: &mut ProofBundle, command: ProofCommand) {
        if let Some(run_id) = command.run_id.as_ref() {
            Self::push_unique(&mut bundle.run_ids, run_id.clone());
        }
        if let Some(evidence_ref) = command.evidence_ref.as_deref() {
            for item in evidence_ref.lines().map(str::trim) {
                if let Some(commit) = item
                    .strip_prefix("commit:")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    Self::push_unique(&mut bundle.commits, commit.to_string());
                } else if let Some(summary) = item
                    .strip_prefix("diff_summary:")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    bundle.diff_summary = Some(summary.to_string());
                }
            }
        }
        bundle.commands.push(command);
    }

    fn record_check(bundle: &mut ProofBundle, check: ProofCheck, is_test_result: bool) {
        if is_test_result {
            bundle.test_results.push(check);
        } else {
            bundle.checks.push(check);
        }
    }

    fn record_review(bundle: &mut ProofBundle, review: ProofReview) {
        Self::push_unique(&mut bundle.review_ids, review.review_id.clone());
        bundle.review_findings.extend(review.findings.clone());
        bundle.followups.extend(review.followups.clone());
        bundle.review_scores.push(review);
    }

    fn record_integration(bundle: &mut ProofBundle, integration: ProofIntegration) {
        bundle.conflicts.extend(integration.conflicts.clone());
        bundle.integration_result = Some(integration);
    }

    fn record_decision(
        bundle: &mut ProofBundle,
        scope: brehon_types::ProofDecisionScope,
        decision: ProofDecision,
    ) {
        match scope {
            brehon_types::ProofDecisionScope::Operator => bundle.operator_decisions.push(decision),
            brehon_types::ProofDecisionScope::Supervisor => {
                bundle.supervisor_decisions.push(decision);
            }
        }
    }

    fn record_blocker(bundle: &mut ProofBundle, blocker: ProofBlocker) {
        if let Some(blocker_id) = blocker.blocker_id.as_ref() {
            if let Some(existing) = bundle
                .blockers
                .iter_mut()
                .find(|candidate| candidate.blocker_id.as_ref() == Some(blocker_id))
            {
                *existing = blocker;
                return;
            }
        }
        bundle.blockers.push(blocker);
    }

    fn apply_to_bundle(existing: Option<ProofBundle>, event: &Event) -> Option<ProofBundle> {
        match &event.kind {
            EventKind::ProofBundleCreated {
                proof_bundle_id,
                task_id,
                run_ids,
                created_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    ProofBundle::empty(proof_bundle_id.clone(), task_id.clone(), *created_at)
                });
                Self::merge_run_ids(&mut bundle, run_ids);
                bundle.updated_at = *created_at;
                Some(bundle)
            }
            EventKind::ProofCommandRecorded {
                proof_bundle_id,
                task_id,
                command,
                recorded_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    Self::base_bundle(proof_bundle_id.clone(), task_id.clone(), event)
                });
                Self::record_command(&mut bundle, command.clone());
                bundle.updated_at = *recorded_at;
                Some(bundle)
            }
            EventKind::ProofCheckRecorded {
                proof_bundle_id,
                task_id,
                check,
                is_test_result,
                recorded_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    Self::base_bundle(proof_bundle_id.clone(), task_id.clone(), event)
                });
                Self::record_check(&mut bundle, check.clone(), *is_test_result);
                bundle.updated_at = *recorded_at;
                Some(bundle)
            }
            EventKind::ProofReviewLinked {
                proof_bundle_id,
                task_id,
                review,
                linked_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    Self::base_bundle(proof_bundle_id.clone(), task_id.clone(), event)
                });
                Self::record_review(&mut bundle, review.clone());
                bundle.updated_at = *linked_at;
                Some(bundle)
            }
            EventKind::ProofIntegrationRecorded {
                proof_bundle_id,
                task_id,
                integration,
                recorded_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    Self::base_bundle(proof_bundle_id.clone(), task_id.clone(), event)
                });
                Self::record_integration(&mut bundle, integration.clone());
                bundle.updated_at = *recorded_at;
                Some(bundle)
            }
            EventKind::ProofDecisionRecorded {
                proof_bundle_id,
                task_id,
                scope,
                decision,
                recorded_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    Self::base_bundle(proof_bundle_id.clone(), task_id.clone(), event)
                });
                Self::record_decision(&mut bundle, *scope, decision.clone());
                bundle.updated_at = *recorded_at;
                Some(bundle)
            }
            EventKind::ProofBlockerRecorded {
                proof_bundle_id,
                task_id,
                blocker,
                recorded_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    Self::base_bundle(proof_bundle_id.clone(), task_id.clone(), event)
                });
                Self::record_blocker(&mut bundle, blocker.clone());
                bundle.updated_at = *recorded_at;
                Some(bundle)
            }
            EventKind::ProofBundleFinalized {
                proof_bundle_id,
                task_id,
                final_status,
                finalized_at,
            } => {
                let mut bundle = existing.unwrap_or_else(|| {
                    Self::base_bundle(proof_bundle_id.clone(), task_id.clone(), event)
                });
                bundle.final_status = *final_status;
                bundle.updated_at = *finalized_at;
                Some(bundle)
            }
            _ => None,
        }
    }

    fn proof_bundle_id(event: &Event) -> Option<&ProofBundleId> {
        match &event.kind {
            EventKind::ProofBundleCreated {
                proof_bundle_id, ..
            }
            | EventKind::ProofCommandRecorded {
                proof_bundle_id, ..
            }
            | EventKind::ProofCheckRecorded {
                proof_bundle_id, ..
            }
            | EventKind::ProofReviewLinked {
                proof_bundle_id, ..
            }
            | EventKind::ProofIntegrationRecorded {
                proof_bundle_id, ..
            }
            | EventKind::ProofDecisionRecorded {
                proof_bundle_id, ..
            }
            | EventKind::ProofBlockerRecorded {
                proof_bundle_id, ..
            }
            | EventKind::ProofBundleFinalized {
                proof_bundle_id, ..
            } => Some(proof_bundle_id),
            _ => None,
        }
    }

    /// Apply one event to the persisted proof projection.
    pub fn apply_event(
        &self,
        event: &Event,
        _event_id: EventId,
    ) -> ProofStoreResult<Option<ProofBundle>> {
        let _guard = self.mutation_lock.lock();
        let Some(proof_bundle_id) = Self::proof_bundle_id(event) else {
            return Ok(None);
        };
        let previous = self.get_bundle_locked(proof_bundle_id)?;
        let Some(bundle) = Self::apply_to_bundle(previous.clone(), event) else {
            return Ok(None);
        };
        self.commit_bundle(previous.as_ref(), &bundle)?;
        Ok(Some(bundle))
    }

    /// Rebuild the persisted proof projection from the event partition.
    pub fn rebuild_from_events(&self, events: &PartitionHandle) -> ProofStoreResult<usize> {
        let _guard = self.mutation_lock.lock();
        let mut bundles: HashMap<ProofBundleId, ProofBundle> = HashMap::new();
        let mut applied = 0usize;

        for item in events.prefix(b"log:") {
            let (_, value) = item.map_err(Self::storage_error)?;
            let envelope = deserialize_event_envelope(&value)
                .map_err(|err| ProofStoreError::Serialization(err.to_string()))?;
            let Some(proof_bundle_id) = Self::proof_bundle_id(&envelope.event).cloned() else {
                continue;
            };
            let previous = bundles.remove(&proof_bundle_id);
            if let Some(bundle) = Self::apply_to_bundle(previous, &envelope.event) {
                bundles.insert(proof_bundle_id, bundle);
                applied += 1;
            }
        }

        let bundles: Vec<ProofBundle> = bundles.into_values().collect();
        self.clear_and_insert_bundles(&bundles)?;
        Ok(applied)
    }

    /// Get a proof bundle by id.
    pub fn get_bundle(
        &self,
        proof_bundle_id: &ProofBundleId,
    ) -> ProofStoreResult<Option<ProofBundle>> {
        self.get_bundle_locked(proof_bundle_id)
    }

    /// Get the current proof bundle for a task.
    pub fn get_bundle_for_task(&self, task_id: &TaskId) -> ProofStoreResult<Option<ProofBundle>> {
        self.indexed_bundle(proof_task_index_key(task_id.as_str()))
    }

    /// Get the current proof bundle for a run.
    pub fn get_bundle_for_run(&self, run_id: &RunId) -> ProofStoreResult<Option<ProofBundle>> {
        self.indexed_bundle(proof_run_index_key(run_id.as_str()))
    }
}

/// Map the outcome of a `spawn_blocking` proof-store operation into a
/// `ProofStoreResult`. The synchronous fjall fsync (`PersistMode::SyncAll`) runs
/// on the blocking pool so it never parks a Tokio worker; a panic inside the
/// closure (e.g. an unexpected fjall failure) surfaces here as a `JoinError`,
/// which we fail closed into a `ProofStoreError::Storage` rather than propagating
/// the panic. This relies on the `panic = "unwind"` profile setting documented in
/// the workspace `Cargo.toml`.
fn map_proof_store_blocking<T>(
    joined: Result<ProofStoreResult<T>, tokio::task::JoinError>,
) -> ProofStoreResult<T> {
    match joined {
        Ok(inner) => inner,
        Err(join_err) => Err(ProofStoreError::Storage(format!(
            "proof store storage task panicked: {join_err}"
        ))),
    }
}

#[async_trait]
impl ProofStore for FjallEventStore {
    async fn rebuild_proof_projection(&self) -> ProofStoreResult<usize> {
        // Bail cheaply before scheduling blocking work when no projection exists.
        if !self.proof_store_available() {
            return Err(proof_projection_unavailable());
        }
        let store = self.clone();
        map_proof_store_blocking(
            tokio::task::spawn_blocking(move || {
                let Some(proof_store) = store.proof_store() else {
                    return Err(proof_projection_unavailable());
                };
                proof_store.rebuild_from_events(store.events_partition())
            })
            .await,
        )
    }

    async fn apply_proof_event(
        &self,
        event: &Event,
        event_id: EventId,
    ) -> ProofStoreResult<Option<ProofBundle>> {
        if !self.proof_store_available() {
            return Err(proof_projection_unavailable());
        }
        let store = self.clone();
        let event = event.clone();
        map_proof_store_blocking(
            tokio::task::spawn_blocking(move || {
                let Some(proof_store) = store.proof_store() else {
                    return Err(proof_projection_unavailable());
                };
                proof_store.apply_event(&event, event_id)
            })
            .await,
        )
    }

    async fn proof_bundle(
        &self,
        proof_bundle_id: &ProofBundleId,
    ) -> ProofStoreResult<Option<ProofBundle>> {
        if !self.proof_store_available() {
            return Err(proof_projection_unavailable());
        }
        let store = self.clone();
        let proof_bundle_id = proof_bundle_id.clone();
        map_proof_store_blocking(
            tokio::task::spawn_blocking(move || {
                let Some(proof_store) = store.proof_store() else {
                    return Err(proof_projection_unavailable());
                };
                proof_store.get_bundle(&proof_bundle_id)
            })
            .await,
        )
    }

    async fn proof_bundle_for_task(
        &self,
        task_id: &TaskId,
    ) -> ProofStoreResult<Option<ProofBundle>> {
        if !self.proof_store_available() {
            return Err(proof_projection_unavailable());
        }
        let store = self.clone();
        let task_id = task_id.clone();
        map_proof_store_blocking(
            tokio::task::spawn_blocking(move || {
                let Some(proof_store) = store.proof_store() else {
                    return Err(proof_projection_unavailable());
                };
                proof_store.get_bundle_for_task(&task_id)
            })
            .await,
        )
    }

    async fn proof_bundle_for_run(&self, run_id: &RunId) -> ProofStoreResult<Option<ProofBundle>> {
        if !self.proof_store_available() {
            return Err(proof_projection_unavailable());
        }
        let store = self.clone();
        let run_id = run_id.clone();
        map_proof_store_blocking(
            tokio::task::spawn_blocking(move || {
                let Some(proof_store) = store.proof_store() else {
                    return Err(proof_projection_unavailable());
                };
                proof_store.get_bundle_for_run(&run_id)
            })
            .await,
        )
    }
}

fn proof_projection_unavailable() -> ProofStoreError {
    ProofStoreError::Storage(
        "proof projection is unavailable; rebuild or archive the proofs partition".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_ports::EventStore;
    use brehon_types::{
        ProofBlockerStatus, ProofBundleStatus, ProofCheckStatus, ProofDecisionScope, ReviewId,
        ReviewScore, ReviewVerdict,
    };
    use chrono::{DateTime, TimeZone, Utc};
    use tempfile::tempdir;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap()
    }

    fn proof_bundle_id() -> ProofBundleId {
        ProofBundleId::new("proof-T-1")
    }

    fn task_id() -> TaskId {
        TaskId::new("T-1")
    }

    fn run_id() -> RunId {
        RunId::new("run-T-1-worker-1")
    }

    fn event(kind: EventKind) -> Event {
        Event {
            kind,
            timestamp: ts(),
            aggregate_id: "T-1".into(),
        }
    }

    fn command() -> ProofCommand {
        ProofCommand {
            run_id: Some(run_id()),
            command: "cargo test -p brehon-types proof".into(),
            cwd: Some("/repo".into()),
            exit_code: Some(0),
            started_at: ts(),
            completed_at: Some(ts()),
            output_summary: Some("passed".into()),
            evidence_ref: None,
        }
    }

    fn check() -> ProofCheck {
        ProofCheck {
            name: "proof tests".into(),
            command: Some("cargo test -p brehon-types proof".into()),
            status: ProofCheckStatus::Passed,
            summary: Some("passed".into()),
            evidence_ref: None,
            checked_at: ts(),
        }
    }

    fn review() -> ProofReview {
        ProofReview {
            review_id: ReviewId::new("review-1"),
            reviewer_id: Some("reviewer-1".into()),
            score: Some(ReviewScore::new(8)),
            verdict: Some(ReviewVerdict::Approve),
            findings: vec!["no blockers".into()],
            followups: vec!["confirm proof store".into()],
            reviewed_at: ts(),
        }
    }

    fn integration() -> ProofIntegration {
        ProofIntegration {
            status: "integrated".into(),
            branch: Some("task/T-1".into()),
            base_branch: Some("main".into()),
            worktree_path: Some("/repo/.worktrees/T-1".into()),
            commit: Some("abc1234".into()),
            summary: Some("merged".into()),
            conflicts: Vec::new(),
            integrated_at: ts(),
        }
    }

    fn decision() -> ProofDecision {
        ProofDecision {
            decision_id: Some("decision-1".into()),
            decided_by: "supervisor".into(),
            decision: "accept".into(),
            reason: Some("proof is complete".into()),
            decided_at: ts(),
        }
    }

    fn blocker(status: ProofBlockerStatus) -> ProofBlocker {
        ProofBlocker {
            blocker_id: Some("blocker-1".into()),
            summary: "missing proof store".into(),
            source: Some("test".into()),
            status,
            created_at: ts(),
            resolved_at: None,
            resolution: None,
        }
    }

    fn proof_events() -> Vec<Event> {
        vec![
            event(EventKind::ProofBundleCreated {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                run_ids: vec![run_id()],
                created_at: ts(),
            }),
            event(EventKind::ProofCommandRecorded {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                command: command(),
                recorded_at: ts(),
            }),
            event(EventKind::ProofCheckRecorded {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                check: check(),
                is_test_result: true,
                recorded_at: ts(),
            }),
            event(EventKind::ProofReviewLinked {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                review: review(),
                linked_at: ts(),
            }),
            event(EventKind::ProofIntegrationRecorded {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                integration: integration(),
                recorded_at: ts(),
            }),
            event(EventKind::ProofDecisionRecorded {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                scope: ProofDecisionScope::Supervisor,
                decision: decision(),
                recorded_at: ts(),
            }),
            event(EventKind::ProofBlockerRecorded {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                blocker: blocker(ProofBlockerStatus::Resolved),
                recorded_at: ts(),
            }),
            event(EventKind::ProofBundleFinalized {
                proof_bundle_id: proof_bundle_id(),
                task_id: task_id(),
                final_status: ProofBundleStatus::Complete,
                finalized_at: ts(),
            }),
        ]
    }

    #[tokio::test]
    async fn proof_projection_rebuilds_and_queries_by_task_and_run() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();
        for event in proof_events() {
            store.append(event).await.unwrap();
        }

        let applied = store.rebuild_proof_projection().await.unwrap();
        assert_eq!(applied, 8);

        let by_task = store
            .proof_bundle_for_task(&task_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_task.proof_bundle_id, proof_bundle_id());
        assert_eq!(by_task.commands.len(), 1);
        assert_eq!(by_task.test_results.len(), 1);
        assert_eq!(by_task.review_scores.len(), 1);
        assert_eq!(by_task.integration_result.unwrap().status, "integrated");
        assert_eq!(by_task.supervisor_decisions.len(), 1);
        assert_eq!(by_task.final_status, ProofBundleStatus::Complete);

        let by_run = store
            .proof_bundle_for_run(&run_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_run.proof_bundle_id, proof_bundle_id());
    }

    #[tokio::test]
    async fn proof_projection_apply_event_updates_existing_bundle() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();
        let created = proof_events().remove(0);
        store
            .apply_proof_event(&created, EventId::new(1))
            .await
            .unwrap();
        store
            .apply_proof_event(
                &event(EventKind::ProofBlockerRecorded {
                    proof_bundle_id: proof_bundle_id(),
                    task_id: task_id(),
                    blocker: blocker(ProofBlockerStatus::Open),
                    recorded_at: ts(),
                }),
                EventId::new(2),
            )
            .await
            .unwrap();
        store
            .apply_proof_event(
                &event(EventKind::ProofBlockerRecorded {
                    proof_bundle_id: proof_bundle_id(),
                    task_id: task_id(),
                    blocker: blocker(ProofBlockerStatus::Resolved),
                    recorded_at: ts(),
                }),
                EventId::new(3),
            )
            .await
            .unwrap();

        let bundle = store
            .proof_bundle(&proof_bundle_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bundle.blockers.len(), 1);
        assert_eq!(bundle.blockers[0].status, ProofBlockerStatus::Resolved);
    }

    #[tokio::test]
    async fn proof_projection_survives_store_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let store = FjallEventStore::new(&path).unwrap();
        for event in proof_events() {
            store.append(event).await.unwrap();
        }
        store.rebuild_proof_projection().await.unwrap();
        store.persist().unwrap();
        drop(store);

        let reopened = FjallEventStore::new(&path).unwrap();
        let bundle = reopened
            .proof_bundle_for_task(&task_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bundle.proof_bundle_id, proof_bundle_id());
        assert_eq!(bundle.run_ids, vec![run_id()]);
        assert_eq!(bundle.final_status, ProofBundleStatus::Complete);
    }
}
