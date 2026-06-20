//! FjallEventStore implementation.
//!
//! This module implements the `EventStore` trait using fjall, a Rust-native
//! LSM-tree embedded database.

use async_trait::async_trait;
use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

use brehon_ports::{EventStore, PortError};
use brehon_types::{ClaimId, Event, EventEnvelope, EventFilter, EventId, QueueClaim, ViewUpdate};

pub use crate::error::StoreError;
use crate::keys::*;
use crate::owner_lock::StoreOwnerLock;
use crate::queries::QueryExecutor;
use crate::queue::QueueManager;
use crate::recovery::RecoveryScanner;
use crate::run_store::RunStoreManager;
use crate::views::ViewManager;

const EVENTS_PARTITION: &str = "events";
const VIEWS_PARTITION: &str = "views";
const QUEUE_PARTITION: &str = "queue";
const META_PARTITION: &str = "meta";
const ARCHIVE_PARTITION: &str = "archive";
const RUNS_PARTITION: &str = "runs";
const PROOFS_PARTITION: &str = "proofs";

/// Map the outcome of a `spawn_blocking` task that performed a fjall mutation
/// into a `Result<T, PortError>`.
///
/// The synchronous fjall fsync (`PersistMode::SyncAll` + `batch.commit()`) is
/// run on the blocking thread pool so it never parks a Tokio worker. A panic
/// inside that closure (e.g. an unexpected fjall failure) surfaces here as a
/// `JoinError`; we fail closed by turning it into a `PortError::Storage` rather
/// than letting the panic propagate to the caller. This relies on the
/// `panic = "unwind"` profile setting (documented in the workspace `Cargo.toml`):
/// a future `panic = "abort"` would convert such a panic into a whole-process
/// abort instead of a recoverable error.
fn map_blocking<T>(
    joined: Result<Result<T, StoreError>, tokio::task::JoinError>,
) -> Result<T, PortError> {
    match joined {
        Ok(inner) => inner.map_err(PortError::from),
        Err(join_err) => Err(PortError::Storage(format!(
            "event store storage task panicked: {join_err}"
        ))),
    }
}

pub struct FjallEventStoreInner {
    _owner_lock: StoreOwnerLock,
    keyspace: Keyspace,
    events: PartitionHandle,
    archive: PartitionHandle,
    meta: PartitionHandle,
    seq: AtomicU64,
    append_lock: Mutex<()>,
    view_manager: ViewManager,
    query_executor: QueryExecutor,
    queue_manager: QueueManager,
    run_store: RunStoreManager,
    proof_store: Option<crate::proof_store::ProofStoreManager>,
}

pub struct FjallEventStore {
    inner: Arc<FjallEventStoreInner>,
}

struct KeyspaceOpen {
    keyspace: Keyspace,
    proof_projection_quarantined: bool,
}

impl std::clone::Clone for FjallEventStore {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl FjallEventStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, StoreError> {
        let path = path.as_ref();
        info!("Opening FjallEventStore at {:?}", path);

        let owner_lock = StoreOwnerLock::acquire(path)?;
        let keyspace_open = Self::open_keyspace_with_optional_projection_recovery(path)?;
        let keyspace = keyspace_open.keyspace;

        let events =
            keyspace.open_partition(EVENTS_PARTITION, PartitionCreateOptions::default())?;
        let views = keyspace.open_partition(VIEWS_PARTITION, PartitionCreateOptions::default())?;
        let queue = keyspace.open_partition(QUEUE_PARTITION, PartitionCreateOptions::default())?;
        let meta = keyspace.open_partition(META_PARTITION, PartitionCreateOptions::default())?;
        let archive =
            keyspace.open_partition(ARCHIVE_PARTITION, PartitionCreateOptions::default())?;
        let runs = keyspace.open_partition(RUNS_PARTITION, PartitionCreateOptions::default())?;
        let mut proof_store =
            match keyspace.open_partition(PROOFS_PARTITION, PartitionCreateOptions::default()) {
                Ok(proofs) => Some(crate::proof_store::ProofStoreManager::new(
                    keyspace.clone(),
                    proofs,
                )),
                Err(err) => {
                    tracing::error!(
                        partition = PROOFS_PARTITION,
                        error = %err,
                        "Fjall proof projection unavailable; continuing without proof store"
                    );
                    None
                }
            };
        if keyspace_open.proof_projection_quarantined {
            if let Some(proof_store_ref) = proof_store.as_ref() {
                match proof_store_ref.rebuild_from_events(&events) {
                    Ok(applied) => {
                        keyspace.persist(PersistMode::SyncAll)?;
                        tracing::warn!(
                            applied,
                            partition = PROOFS_PARTITION,
                            "Rebuilt quarantined Fjall proof projection from event log"
                        );
                    }
                    Err(err) => {
                        tracing::error!(
                            partition = PROOFS_PARTITION,
                            error = %err,
                            "Failed to rebuild quarantined Fjall proof projection; continuing without proof store"
                        );
                        proof_store = None;
                    }
                }
            }
        }

        let seq = Self::load_seq(&meta, &events)?;

        // Fail closed: a binary must refuse a store written by a newer schema
        // rather than silently misread it. The migration runner owns the on-disk
        // version encoding; when it stamps a fresh/unversioned store, persist the
        // stamp immediately so a crash after open does not leave the store
        // ambiguously unversioned.
        let migration_report =
            crate::migrations::MigrationRunner::new(meta.clone()).run_migrations()?;
        if migration_report.migrations_applied() {
            keyspace.persist(PersistMode::SyncAll)?;
            debug!(
                "Stamped fresh store schema version {}",
                crate::migrations::CURRENT_SCHEMA_VERSION
            );
        }

        let view_manager = ViewManager::new(keyspace.clone(), views.clone());
        let query_executor = QueryExecutor::new(events.clone(), meta.clone());
        let queue_manager = QueueManager::new(
            keyspace.clone(),
            queue.clone(),
            Self::queue_lock_scope(path),
        );
        let run_store = RunStoreManager::new(keyspace.clone(), runs);
        let views_watermark = Self::load_views_watermark(&meta)?;
        let events_watermark = Self::load_high_water_mark(&events)?;
        let views_are_valid = view_manager.validate_views(events_watermark)?;
        let rebuilt_views = if views_watermark != events_watermark || !views_are_valid {
            let count = view_manager.rebuild_views_from_events(&events)?;
            Self::save_views_watermark(&meta, events_watermark)?;
            if views_watermark != events_watermark {
                debug!(
                    "Rebuilt views because watermark changed from {views_watermark} to {events_watermark}"
                );
            }
            if !views_are_valid {
                debug!("Rebuilt views because existing persisted views were invalid");
            }
            count
        } else {
            0
        };
        if rebuilt_views > 0 {
            debug!("Rebuilt {rebuilt_views} views from events during startup");
        }

        let recovery_view_manager = ViewManager::new(keyspace.clone(), views.clone());
        let recovery_report = RecoveryScanner::new(
            events.clone(),
            recovery_view_manager,
            queue.clone(),
            queue_manager.active_lease_epoch(),
            queue_manager.active_lease_elapsed_ms(),
        )
        .scan()?;
        if recovery_report.recovered_count > 0 {
            keyspace.persist(PersistMode::SyncAll)?;
        }
        if recovery_report.has_issues() {
            tracing::warn!(
                orphaned_tasks = recovery_report.orphaned_tasks.len(),
                prepared_merges = recovery_report.prepared_merges.len(),
                expired_claims = recovery_report.expired_claims.len(),
                cleaned_expired_claims = recovery_report.recovered_count,
                "Startup recovery scan found issues"
            );
        } else {
            debug!("Startup recovery scan found no issues");
        }

        let inner = Arc::new(FjallEventStoreInner {
            _owner_lock: owner_lock,
            keyspace,
            events,
            archive,
            meta,
            seq: AtomicU64::new(seq),
            append_lock: Mutex::new(()),
            view_manager,
            query_executor,
            queue_manager,
            run_store,
            proof_store,
        });

        Ok(Self { inner })
    }

    fn open_keyspace_with_optional_projection_recovery(
        path: &Path,
    ) -> Result<KeyspaceOpen, StoreError> {
        match Config::new(path).open() {
            Ok(keyspace) => Ok(KeyspaceOpen {
                keyspace,
                proof_projection_quarantined: false,
            }),
            Err(err) => {
                let original_error = err.to_string();
                let proof_path = path.join("partitions").join(PROOFS_PARTITION);
                if !proof_path.exists() {
                    return Err(StoreError::Storage(original_error));
                }

                let quarantine_path = Self::unique_quarantine_path(path, &proof_path);
                if let Some(parent) = quarantine_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if let Err(quarantine_err) = std::fs::rename(&proof_path, &quarantine_path) {
                    return Err(StoreError::Storage(format!(
                        "Fjall keyspace open failed ({original_error}) and optional proof projection could not be quarantined: {quarantine_err}"
                    )));
                }

                tracing::error!(
                    partition = PROOFS_PARTITION,
                    original_error = %original_error,
                    quarantine_path = %quarantine_path.display(),
                    "Quarantined optional Fjall proof projection after keyspace recovery failed"
                );

                match Config::new(path).open() {
                    Ok(keyspace) => Ok(KeyspaceOpen {
                        keyspace,
                        proof_projection_quarantined: true,
                    }),
                    Err(retry_err) => {
                        let retry_error = retry_err.to_string();
                        if !proof_path.exists() {
                            if let Err(restore_err) = std::fs::rename(&quarantine_path, &proof_path)
                            {
                                tracing::error!(
                                    partition = PROOFS_PARTITION,
                                    quarantine_path = %quarantine_path.display(),
                                    restore_error = %restore_err,
                                    "Failed to restore quarantined Fjall proof projection after retry failed"
                                );
                            }
                        }
                        Err(StoreError::Storage(format!(
                            "Fjall keyspace open failed after quarantining optional proof projection: {retry_error}; original error: {original_error}"
                        )))
                    }
                }
            }
        }
    }

    fn unique_quarantine_path(db_path: &Path, partition_path: &Path) -> PathBuf {
        let parent = db_path.join("quarantine");
        let name = partition_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("partition");
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        for attempt in 0..1000 {
            let suffix = if attempt == 0 {
                format!("quarantined.{stamp}.{}", std::process::id())
            } else {
                format!("quarantined.{stamp}.{}.{}", std::process::id(), attempt)
            };
            let candidate = parent.join(format!("{name}.{suffix}"));
            if !candidate.exists() {
                return candidate;
            }
        }
        parent.join(format!(
            "{name}.quarantined.{stamp}.{}.fallback",
            std::process::id()
        ))
    }

    fn queue_lock_scope(path: &Path) -> String {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, PortError> {
        Self::new(path).map_err(PortError::from)
    }

    pub(crate) fn run_store(&self) -> &RunStoreManager {
        &self.inner.run_store
    }

    pub fn proof_store_available(&self) -> bool {
        self.inner.proof_store.is_some()
    }

    pub(crate) fn proof_store(&self) -> Option<&crate::proof_store::ProofStoreManager> {
        self.inner.proof_store.as_ref()
    }

    pub(crate) fn events_partition(&self) -> &PartitionHandle {
        &self.inner.events
    }

    fn load_seq(meta: &PartitionHandle, events: &PartitionHandle) -> Result<u64, StoreError> {
        let persisted_seq = match meta.get(KEY_META_SEQ)? {
            Some(bytes) => {
                let bytes_vec = bytes.to_vec();
                let arr: [u8; 8] = bytes_vec[..]
                    .try_into()
                    .map_err(|_| StoreError::Storage("Invalid sequence number format".into()))?;
                u64::from_be_bytes(arr)
            }
            None => 0,
        };
        let high_water_mark = Self::load_high_water_mark(events)?;
        Ok(std::cmp::max(persisted_seq, high_water_mark))
    }

    fn load_views_watermark(meta: &PartitionHandle) -> Result<u64, StoreError> {
        match meta.get(KEY_META_VIEWS_LAST_EVENT_ID)? {
            Some(bytes) => {
                let bytes_vec = bytes.to_vec();
                let arr: [u8; 8] = bytes_vec[..]
                    .try_into()
                    .map_err(|_| StoreError::Storage("Invalid view watermark format".into()))?;
                Ok(u64::from_be_bytes(arr))
            }
            None => Ok(0),
        }
    }

    fn save_views_watermark(meta: &PartitionHandle, watermark: u64) -> Result<(), StoreError> {
        meta.insert(KEY_META_VIEWS_LAST_EVENT_ID, watermark.to_be_bytes())?;
        Ok(())
    }

    fn load_high_water_mark(events: &PartitionHandle) -> Result<u64, StoreError> {
        let last_log_entry = events
            .prefix(b"log:")
            .next_back()
            .transpose()
            .map_err(|e| StoreError::Storage(e.to_string()))?;

        Ok(last_log_entry
            .and_then(|(key, _value)| parse_seq_from_log_key(&key))
            .unwrap_or(0))
    }

    fn next_event_id(&self) -> EventId {
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst);
        EventId::new(seq + 1)
    }

    fn restore_seq_on_error<T>(
        &self,
        seq_before: u64,
        result: Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        // Safe because all append paths call this helper while holding append_lock,
        // so there is no concurrent writer that could race this rollback.
        if result.is_err() {
            self.inner.seq.store(seq_before, Ordering::SeqCst);
        }
        result
    }

    fn get_current_seq(&self) -> u64 {
        self.inner.seq.load(Ordering::SeqCst)
    }

    fn serialize_event(event: &EventEnvelope) -> Result<Vec<u8>, StoreError> {
        Ok(serde_json::to_vec(event)?)
    }

    fn deserialize_event(bytes: &[u8]) -> Result<EventEnvelope, StoreError> {
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn close(self) -> Result<(), StoreError> {
        self.inner.keyspace.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    pub fn persist(&self) -> Result<(), StoreError> {
        self.inner.keyspace.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    /// Return the number of events in the hot log partition.
    pub fn event_count(&self) -> Result<u64, StoreError> {
        let mut count = 0u64;
        for item in self.inner.events.prefix(b"log:") {
            let _ = item.map_err(|e| StoreError::Storage(e.to_string()))?;
            count += 1;
        }
        Ok(count)
    }

    /// Archive (move) all events with sequence number <= `cutoff_seq` from the hot
    /// events partition to the archive partition. Also removes associated index keys.
    ///
    /// Returns the number of events archived.
    pub fn archive_events_before(&self, cutoff_seq: u64) -> Result<u64, StoreError> {
        let _guard = self.append_lock();
        let mut batch = self
            .inner
            .keyspace
            .batch()
            .durability(Some(PersistMode::SyncAll));

        let mut archived = 0u64;

        // Archive log entries. Keys are lexicographically sorted by seq,
        // so we can break early once we pass the cutoff.
        for item in self.inner.events.prefix(b"log:") {
            let (key, value) = item.map_err(|e| StoreError::Storage(e.to_string()))?;
            if let Some(seq) = parse_seq_from_log_key(&key) {
                if seq <= cutoff_seq {
                    let archive_k = archive_key(seq);
                    batch.insert(&self.inner.archive, &archive_k, value);
                    batch.remove(&self.inner.events, key);
                    archived += 1;
                } else {
                    break;
                }
            }
        }

        // Remove index entries for archived events
        for item in self.inner.events.prefix(b"index:") {
            let (key, _value) = item.map_err(|e| StoreError::Storage(e.to_string()))?;
            if let Some(seq) = parse_seq_from_index_key(&key) {
                if seq <= cutoff_seq {
                    batch.remove(&self.inner.events, key);
                }
            }
        }

        // Update index_start_seq to be past the cutoff so index scans don't
        // look into archived territory.
        let new_index_start = cutoff_seq + 1;
        batch.insert(
            &self.inner.meta,
            KEY_META_INDEX_START_SEQ,
            new_index_start.to_be_bytes(),
        );

        batch.commit()?;
        info!(archived, cutoff_seq, "Archived events before cutoff");
        Ok(archived)
    }

    /// Delete idempotency keys that reference events with sequence number <=
    /// `cutoff_seq`. Safe to call after archiving because the referenced events
    /// are no longer in the hot path.
    ///
    /// Returns the number of idempotency keys removed.
    pub fn prune_idempotency_keys(&self, cutoff_seq: u64) -> Result<u64, StoreError> {
        let mut removed = 0u64;
        let prefix = b"meta:idempotency:";

        // Collect keys first to avoid modification-during-iteration issues.
        let mut to_remove: Vec<Vec<u8>> = Vec::new();

        for item in self.inner.meta.prefix(prefix) {
            let (key, value) = item.map_err(|e| StoreError::Storage(e.to_string()))?;
            if value.len() >= 8 {
                let arr: [u8; 8] = value[..8]
                    .try_into()
                    .map_err(|_| StoreError::Storage("Invalid idempotency value".into()))?;
                let event_id = u64::from_be_bytes(arr);
                if event_id <= cutoff_seq {
                    to_remove.push(key.to_vec());
                }
            }
        }

        if !to_remove.is_empty() {
            let mut batch = self
                .inner
                .keyspace
                .batch()
                .durability(Some(PersistMode::SyncAll));
            for key in &to_remove {
                batch.remove(&self.inner.meta, key);
            }
            batch.commit()?;
            removed = to_remove.len() as u64;
        }

        info!(removed, cutoff_seq, "Pruned idempotency keys");
        Ok(removed)
    }

    fn check_idempotency(&self, key: Option<&str>) -> Result<Option<EventId>, StoreError> {
        if let Some(key_str) = key {
            let key_bytes = idempotency_key(key_str);
            if let Some(bytes) = self.inner.meta.get(&key_bytes)? {
                let bytes_vec = bytes.to_vec();
                let arr: [u8; 8] = bytes_vec[..]
                    .try_into()
                    .map_err(|_| StoreError::Storage("Invalid idempotency key format".into()))?;
                return Ok(Some(EventId::new(u64::from_be_bytes(arr))));
            }
        }
        Ok(None)
    }

    fn queue_materialized(&self, key: &str) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .meta
            .get(queue_materialization_key(key))?
            .is_some())
    }

    fn build_event_envelope(
        event: &Event,
        event_id: EventId,
        idempotency_key: Option<&str>,
    ) -> EventEnvelope {
        EventEnvelope {
            event: event.clone(),
            event_id,
            correlation_id: None,
            causation_id: None,
            idempotency_key: idempotency_key.map(brehon_types::IdempotencyKey::new),
        }
    }

    fn append_lock(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.inner.append_lock.lock()
    }

    fn make_index_keys(&self, event: &Event, seq: u64) -> Vec<Vec<u8>> {
        let mut keys = Vec::new();

        if let Some(task_id) = event.kind.task_id() {
            keys.push(task_index_key(task_id, seq));
        }
        if let Some(review_id) = event.kind.review_id() {
            keys.push(review_index_key(review_id, seq));
        }
        if let Some(agent_id) = event.kind.agent_id() {
            keys.push(agent_index_key(agent_id, seq));
        }

        keys
    }

    fn set_index_start_seq_if_needed(
        &self,
        batch: &mut fjall::Batch,
        seq: u64,
    ) -> Result<(), StoreError> {
        let existing = self.inner.meta.get(KEY_META_INDEX_START_SEQ)?;
        if existing.is_none() {
            batch.insert(
                &self.inner.meta,
                KEY_META_INDEX_START_SEQ,
                seq.to_be_bytes(),
            );
        }

        Ok(())
    }

    fn append_event_inner_locked(
        &self,
        event: &Event,
        idempotency_key_option: Option<&str>,
    ) -> Result<EventId, StoreError> {
        if let Some(existing_id) = self.check_idempotency(idempotency_key_option)? {
            debug!(
                "Returning existing event id {} for idempotency key",
                existing_id
            );
            self.inner
                .seq
                .fetch_max(existing_id.as_u64(), Ordering::SeqCst);
            return Ok(existing_id);
        }

        let seq_before = self.get_current_seq();
        let result = (|| {
            let event_id = self.next_event_id();
            let seq = event_id.as_u64();

            let envelope = Self::build_event_envelope(event, event_id, idempotency_key_option);

            let key = log_key(seq);
            let value = Self::serialize_event(&envelope)?;

            let mut batch = self
                .inner
                .keyspace
                .batch()
                .durability(Some(PersistMode::SyncAll));
            batch.insert(&self.inner.events, &key, &value);
            let index_keys = self.make_index_keys(event, seq);
            for index_key in index_keys.iter() {
                batch.insert(&self.inner.events, index_key, b"");
            }
            if !index_keys.is_empty() {
                self.set_index_start_seq_if_needed(&mut batch, seq)?;
            }
            batch.insert(&self.inner.meta, KEY_META_SEQ, seq.to_be_bytes());

            if let Some(key_str) = idempotency_key_option {
                batch.insert(
                    &self.inner.meta,
                    idempotency_key(key_str),
                    event_id.as_u64().to_be_bytes(),
                );
            }

            batch.commit()?;

            debug!("Appended event {} to log", event_id);
            Ok(event_id)
        })();

        self.restore_seq_on_error(seq_before, result)
    }

    fn append_and_enqueue_inner_locked(
        &self,
        event: &Event,
        queue: &str,
        item_id: &str,
        enqueue_idempotency_key: Option<&str>,
    ) -> Result<EventId, StoreError> {
        if let Some(existing_id) = self.check_idempotency(enqueue_idempotency_key)? {
            if let Some(key) = enqueue_idempotency_key {
                if !self.queue_materialized(key)? {
                    // Recovery path: older partial writes may have persisted the
                    // ReviewRequested event without recording queue materialization
                    // or secondary indexes. Rebuild all missing artifacts without
                    // re-appending the already-durable event.
                    let mut batch = self
                        .inner
                        .keyspace
                        .batch()
                        .durability(Some(PersistMode::SyncAll));
                    batch.insert(
                        &self.inner.queue_manager.queue,
                        queue_key(queue, existing_id.as_u64()),
                        item_id.as_bytes(),
                    );
                    batch.insert(
                        &self.inner.meta,
                        queue_materialization_key(key),
                        existing_id.as_u64().to_be_bytes(),
                    );
                    let index_keys = self.make_index_keys(event, existing_id.as_u64());
                    for index_key in index_keys.iter() {
                        batch.insert(&self.inner.events, &index_key[..], b"");
                    }
                    if !index_keys.is_empty() {
                        self.set_index_start_seq_if_needed(&mut batch, existing_id.as_u64())?;
                    }
                    batch.commit()?;
                }
            }
            // Ensure the in-memory sequence counter is at least past the
            // recovered event so the next append does not reuse its seq.
            self.inner
                .seq
                .fetch_max(existing_id.as_u64(), Ordering::SeqCst);
            return Ok(existing_id);
        }

        let seq_before = self.get_current_seq();
        let result = (|| {
            let event_id = self.next_event_id();
            let seq = event_id.as_u64();
            let envelope = Self::build_event_envelope(event, event_id, enqueue_idempotency_key);
            let key = log_key(seq);
            let value = Self::serialize_event(&envelope)?;

            let mut batch = self
                .inner
                .keyspace
                .batch()
                .durability(Some(PersistMode::SyncAll));
            batch.insert(&self.inner.events, &key, &value);
            let index_keys = self.make_index_keys(event, seq);
            for index_key in index_keys.iter() {
                batch.insert(&self.inner.events, &index_key[..], b"");
            }
            if !index_keys.is_empty() {
                self.set_index_start_seq_if_needed(&mut batch, seq)?;
            }
            batch.insert(&self.inner.meta, KEY_META_SEQ, seq.to_be_bytes());
            batch.insert(
                &self.inner.queue_manager.queue,
                queue_key(queue, seq),
                item_id.as_bytes(),
            );

            if let Some(key_str) = enqueue_idempotency_key {
                batch.insert(
                    &self.inner.meta,
                    idempotency_key(key_str),
                    event_id.as_u64().to_be_bytes(),
                );
                batch.insert(
                    &self.inner.meta,
                    queue_materialization_key(key_str),
                    event_id.as_u64().to_be_bytes(),
                );
            }

            batch.commit()?;

            debug!(
                "Appended event {} and enqueued item {} in {}",
                event_id, item_id, queue
            );
            Ok(event_id)
        })();

        self.restore_seq_on_error(seq_before, result)
    }

    fn append_atomic_inner_locked(
        &self,
        events: Vec<Event>,
        views: Vec<ViewUpdate>,
    ) -> Result<Vec<EventId>, StoreError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let seq_before = self.get_current_seq();
        let has_view_updates = !views.is_empty();
        let result = (|| {
            let mut batch = self
                .inner
                .keyspace
                .batch()
                .durability(Some(PersistMode::SyncAll));
            let mut event_ids = Vec::with_capacity(events.len());
            let mut first_indexed_seq = None;

            for event in &events {
                let event_id = self.next_event_id();
                let seq = event_id.as_u64();
                let envelope = Self::build_event_envelope(event, event_id, None);
                let key = log_key(seq);
                let value = Self::serialize_event(&envelope)?;
                batch.insert(&self.inner.events, &key, &value);
                let index_keys = self.make_index_keys(event, seq);
                if first_indexed_seq.is_none() && !index_keys.is_empty() {
                    first_indexed_seq = Some(seq);
                }
                for index_key in index_keys.iter() {
                    batch.insert(&self.inner.events, &index_key[..], b"");
                }
                event_ids.push(event_id);
            }

            if let Some(seq) = first_indexed_seq {
                self.set_index_start_seq_if_needed(&mut batch, seq)?;
            }

            let final_seq = event_ids
                .last()
                .map(|event_id| event_id.as_u64())
                .expect("events is not empty");
            batch.insert(&self.inner.meta, KEY_META_SEQ, final_seq.to_be_bytes());
            if has_view_updates {
                batch.insert(
                    &self.inner.meta,
                    KEY_META_VIEWS_LAST_EVENT_ID,
                    final_seq.to_be_bytes(),
                );
            }
            self.inner.view_manager.stage_updates(&views, &mut batch)?;
            batch.commit()?;

            Ok(event_ids)
        })();

        self.restore_seq_on_error(seq_before, result)
    }

    /// Scan the idempotency index and SyncAll-commit removal of keys whose
    /// referenced event predates `cutoff`. Synchronous (the scan and fsync run on
    /// a blocking thread); takes no append_lock so ordering vs appends is
    /// unaffected.
    fn expire_idempotency_keys_inner(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<usize, StoreError> {
        let prefix = b"meta:idempotency:";
        let mut to_remove: Vec<Vec<u8>> = Vec::new();

        for item in self.inner.meta.prefix(prefix) {
            let (key, value) = item.map_err(|e| StoreError::Storage(e.to_string()))?;
            if value.len() >= 8 {
                let arr: [u8; 8] = value[..8]
                    .try_into()
                    .map_err(|_| StoreError::Storage("Invalid idempotency value".into()))?;
                let event_id = u64::from_be_bytes(arr);
                let log_k = log_key(event_id);
                let event_timestamp = if let Ok(Some(bytes)) = self.inner.events.get(&log_k) {
                    Self::deserialize_event(&bytes)
                        .ok()
                        .map(|e| e.event.timestamp)
                } else {
                    // Fallback: check archive partition for the event.
                    let archive_k = archive_key(event_id);
                    self.inner
                        .archive
                        .get(&archive_k)
                        .ok()
                        .flatten()
                        .and_then(|bytes| Self::deserialize_event(&bytes).ok())
                        .map(|e| e.event.timestamp)
                };

                match event_timestamp {
                    Some(ts) if ts < cutoff => {
                        to_remove.push(key.to_vec());
                    }
                    None => {
                        // Event not found in hot log or archive; key is orphaned.
                        to_remove.push(key.to_vec());
                    }
                    _ => {}
                }
            }
        }

        if to_remove.is_empty() {
            return Ok(0);
        }

        let mut batch = self
            .inner
            .keyspace
            .batch()
            .durability(Some(PersistMode::SyncAll));
        for key in &to_remove {
            batch.remove(&self.inner.meta, key);
        }
        batch
            .commit()
            .map_err(|e| StoreError::Storage(e.to_string()))?;
        Ok(to_remove.len())
    }
}

#[async_trait]
impl EventStore for FjallEventStore {
    async fn append(&self, event: Event) -> Result<EventId, PortError> {
        // The synchronous inner fn fsyncs under `append_lock`; run it on the
        // blocking pool so the fsync never parks a Tokio worker. The std
        // `MutexGuard` is created and dropped entirely inside the closure on the
        // blocking thread, so it is never `Send`-required nor seen by async code.
        let store = self.clone();
        map_blocking(
            tokio::task::spawn_blocking(move || {
                let _guard = store.append_lock();
                store.append_event_inner_locked(&event, None)
            })
            .await,
        )
    }

    async fn append_atomic(
        &self,
        events: Vec<Event>,
        views: Vec<ViewUpdate>,
    ) -> Result<Vec<EventId>, PortError> {
        let store = self.clone();
        map_blocking(
            tokio::task::spawn_blocking(move || {
                let _guard = store.append_lock();
                store.append_atomic_inner_locked(events, views)
            })
            .await,
        )
    }

    async fn append_and_enqueue(
        &self,
        event: Event,
        queue: &str,
        item_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<EventId, PortError> {
        // Borrowed args must be owned to cross the 'static spawn_blocking boundary.
        let store = self.clone();
        let queue = queue.to_owned();
        let item_id = item_id.to_owned();
        let idempotency_key = idempotency_key.map(str::to_owned);
        map_blocking(
            tokio::task::spawn_blocking(move || {
                let _guard = store.append_lock();
                store.append_and_enqueue_inner_locked(
                    &event,
                    &queue,
                    &item_id,
                    idempotency_key.as_deref(),
                )
            })
            .await,
        )
    }

    async fn query(&self, filter: EventFilter) -> Result<Vec<Event>, PortError> {
        self.inner
            .query_executor
            .execute(&filter)
            .map_err(PortError::from)
    }

    async fn stream(
        &self,
        since: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<(Event, EventId)>, PortError> {
        let start_seq = since.map(|id| id.as_u64() + 1).unwrap_or(1);
        let current_seq = self.get_current_seq();

        if start_seq > current_seq {
            return Ok(Vec::new());
        }

        let end_seq = std::cmp::min(start_seq + limit as u64, current_seq + 1);
        let mut events = Vec::with_capacity((end_seq - start_seq) as usize);

        for seq in start_seq..end_seq {
            let key = log_key(seq);
            let bytes = if let Some(bytes) = self
                .inner
                .events
                .get(&key)
                .map_err(|e| PortError::Storage(e.to_string()))?
            {
                Some(bytes)
            } else {
                // Fallback: event may have been archived.
                let archive_k = archive_key(seq);
                self.inner
                    .archive
                    .get(&archive_k)
                    .map_err(|e| PortError::Storage(e.to_string()))?
            };

            if let Some(bytes) = bytes {
                let envelope = Self::deserialize_event(&bytes)
                    .map_err(|e| PortError::Storage(e.to_string()))?;
                events.push((envelope.event, envelope.event_id));
            }
        }

        Ok(events)
    }

    async fn claim_next(
        &self,
        queue: &str,
        consumer: &str,
        lease_for: Duration,
    ) -> Result<Option<QueueClaim>, PortError> {
        // The QueueManager fsyncs under its own `claim_lock`; keep that fsync off
        // the runtime worker by delegating on the blocking pool. The `claim_lock`
        // guard is taken inside the closure on the blocking thread.
        let store = self.clone();
        let queue = queue.to_owned();
        let consumer = consumer.to_owned();
        map_blocking(
            tokio::task::spawn_blocking(move || {
                store
                    .inner
                    .queue_manager
                    .claim_next(&queue, &consumer, lease_for)
            })
            .await,
        )
    }

    async fn ack_claim(&self, claim_id: &ClaimId) -> Result<(), PortError> {
        let store = self.clone();
        let claim_id = claim_id.clone();
        map_blocking(
            tokio::task::spawn_blocking(move || store.inner.queue_manager.ack_claim(&claim_id))
                .await,
        )
    }

    async fn renew_claim(&self, claim_id: &ClaimId, lease_for: Duration) -> Result<(), PortError> {
        let store = self.clone();
        let claim_id = claim_id.clone();
        map_blocking(
            tokio::task::spawn_blocking(move || {
                store.inner.queue_manager.renew_claim(&claim_id, lease_for)
            })
            .await,
        )
    }

    async fn high_water_mark(&self) -> Result<EventId, PortError> {
        Ok(EventId::new(self.get_current_seq()))
    }

    async fn retain_events(&self, before: EventId) -> Result<usize, PortError> {
        let cutoff = before.as_u64().saturating_sub(1);
        // archive_events_before takes `append_lock` and SyncAll-commits; the
        // append_lock acquisition stays inside the closure on the blocking thread.
        let store = self.clone();
        let archived = map_blocking(
            tokio::task::spawn_blocking(move || store.archive_events_before(cutoff)).await,
        )?;
        Ok(archived as usize)
    }

    async fn expire_idempotency_keys(&self, older_than: Duration) -> Result<usize, PortError> {
        // Compute the cutoff up front, then run the meta scan + SyncAll commit on
        // the blocking pool so the fsync never parks a Tokio worker.
        let cutoff = chrono::Utc::now()
            .checked_sub_signed(
                chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::MAX),
            )
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);

        let store = self.clone();
        map_blocking(
            tokio::task::spawn_blocking(move || store.expire_idempotency_keys_inner(cutoff)).await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_ports::ProofStore;
    use brehon_types::{
        EventKind, ProofBundleId, ReviewStatus, TaskId, TaskStatus, TaskView, ViewOperation,
        ViewType, ViewUpdate,
    };
    use std::collections::HashSet;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_append_and_stream() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: "T001".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T001".into(),
        };

        let event_id = store.append(event.clone()).await.unwrap();
        assert!(event_id.as_u64() > 0);

        let events = store.stream(None, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0.kind, event.kind);
    }

    #[test]
    fn fresh_store_stamps_current_schema_version() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("db");
        let store = FjallEventStore::new(&db_path).unwrap();
        drop(store);

        // Reopen the meta partition directly and confirm the stamp uses the same
        // little-endian encoding the version guard reads on open.
        let keyspace = Config::new(&db_path).open().unwrap();
        let meta = keyspace
            .open_partition(META_PARTITION, PartitionCreateOptions::default())
            .unwrap();
        let stamped = meta
            .get(KEY_META_SCHEMA_VERSION)
            .unwrap()
            .expect("fresh store must stamp the schema version");
        let arr: [u8; 8] = stamped.as_ref().try_into().unwrap();
        assert_eq!(
            u64::from_le_bytes(arr),
            crate::migrations::CURRENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn refuses_to_open_store_written_by_newer_schema() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("db");

        // Create the store, then stamp a future schema version into the meta
        // partition out of band to simulate a newer binary having written it.
        let store = FjallEventStore::new(&db_path).unwrap();
        store.close().unwrap();

        let future_version = crate::migrations::CURRENT_SCHEMA_VERSION + 1;
        {
            let keyspace = Config::new(&db_path).open().unwrap();
            let meta = keyspace
                .open_partition(META_PARTITION, PartitionCreateOptions::default())
                .unwrap();
            meta.insert(KEY_META_SCHEMA_VERSION, future_version.to_le_bytes())
                .unwrap();
            keyspace.persist(PersistMode::SyncAll).unwrap();
        }

        // A downgraded binary must fail closed rather than misread the store.
        match FjallEventStore::new(&db_path) {
            Ok(_) => panic!("opening a newer-schema store must fail closed"),
            Err(err) => assert!(
                matches!(
                    err,
                    StoreError::VersionMismatch { actual, .. } if actual == future_version
                ),
                "unexpected error opening newer-schema store: {err:?}"
            ),
        }
    }

    #[tokio::test]
    async fn opens_and_appends_when_proof_partition_is_unavailable() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("db");
        let store = FjallEventStore::new(&db_path).unwrap();
        assert!(store.proof_store_available());
        store.close().unwrap();

        let proofs_path = db_path.join("partitions").join(PROOFS_PARTITION);
        std::fs::remove_dir_all(&proofs_path).unwrap();
        std::fs::write(&proofs_path, b"not a partition directory").unwrap();

        let reopened = FjallEventStore::new(&db_path)
            .expect("core event store should open without proof projection");
        assert!(!reopened.proof_store_available());

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: "T-no-proof".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T-no-proof".into(),
        };

        let event_id = reopened.append(event.clone()).await.unwrap();
        assert!(event_id.as_u64() > 0);
        let events = reopened.stream(None, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0.kind, event.kind);
    }

    #[tokio::test]
    async fn quarantines_and_rebuilds_corrupt_proof_partition_that_blocks_keyspace_open() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("db");
        let store = FjallEventStore::new(&db_path).unwrap();
        let task_id = TaskId::new("T-corrupt-proof");
        let proof_bundle_id = ProofBundleId::new("proof-T-corrupt-proof");
        let now = chrono::Utc::now();
        store
            .append(Event {
                kind: EventKind::ProofBundleCreated {
                    proof_bundle_id: proof_bundle_id.clone(),
                    task_id: task_id.clone(),
                    run_ids: Vec::new(),
                    created_at: now,
                },
                timestamp: now,
                aggregate_id: task_id.as_str().to_string(),
            })
            .await
            .unwrap();
        store.rebuild_proof_projection().await.unwrap();
        store.close().unwrap();

        let proof_path = db_path.join("partitions").join(PROOFS_PARTITION);
        std::fs::remove_dir_all(proof_path.join("segments")).unwrap();
        std::fs::create_dir(proof_path.join("segments")).unwrap();
        std::fs::write(
            proof_path.join("levels"),
            [
                0x4c, 0x53, 0x4d, 0x02, 0x07, 0, 0, 0, 0x04, 0, 0, 0, 0, 0, 0, 0, 0x08, 0, 0, 0, 0,
                0, 0, 0, 0x07, 0, 0, 0, 0, 0, 0, 0, 0x06, 0, 0, 0, 0, 0, 0, 0, 0x05, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
        )
        .unwrap();

        let reopened = FjallEventStore::new(&db_path)
            .expect("proof projection corruption should not block core store open");
        assert!(reopened.proof_store_available());
        let bundle = reopened
            .proof_bundle_for_task(&task_id)
            .await
            .unwrap()
            .expect("proof projection should rebuild from event log");
        assert_eq!(bundle.proof_bundle_id, proof_bundle_id);

        let partition_names: Vec<String> = std::fs::read_dir(db_path.join("quarantine"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            partition_names
                .iter()
                .any(|name| name.starts_with("proofs.quarantined.")),
            "corrupt proof partition should be retained under a quarantine name"
        );
    }

    #[test]
    fn append_event_inner_locked_idempotent_reuses_and_advances_seq() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();
        let existing_id = EventId::new(25);

        store
            .inner
            .meta
            .insert(
                idempotency_key("replay:event"),
                existing_id.as_u64().to_be_bytes(),
            )
            .unwrap();

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: "T-idempo".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T-idempo".into(),
        };

        let event_id = store
            .append_event_inner_locked(&event, Some("replay:event"))
            .unwrap();
        assert_eq!(event_id, existing_id);
        assert_eq!(store.get_current_seq(), 25);
    }

    #[tokio::test]
    async fn test_event_ordering() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        let event1 = Event {
            kind: EventKind::TaskCreated {
                task_id: "T001".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T001".into(),
        };
        let event2 = Event {
            kind: EventKind::TaskCreated {
                task_id: "T002".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T002".into(),
        };

        let id1 = store.append(event1).await.unwrap();
        let id2 = store.append(event2).await.unwrap();

        assert!(id2 > id1);

        let events = store.stream(None, 10).await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn test_persistence() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        let store = FjallEventStore::new(&path).unwrap();
        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: "T001".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T001".into(),
        };
        let _ = store.append(event).await.unwrap();
        store.persist().unwrap();
        drop(store);

        let store2 = FjallEventStore::new(&path).unwrap();
        let events = store2.stream(None, 10).await.unwrap();
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn test_claim_and_ack() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        store
            .inner
            .queue_manager
            .enqueue_item("test-queue", "item-1", 1)
            .unwrap();

        let claim = store
            .claim_next("test-queue", "consumer-1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(claim.is_some());
        assert_eq!(claim.as_ref().unwrap().item_id, "item-1");

        let claim2 = store
            .claim_next("test-queue", "consumer-2", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(claim2.is_none());

        store
            .ack_claim(&claim.as_ref().unwrap().claim_id)
            .await
            .unwrap();

        let claim3 = store
            .claim_next("test-queue", "consumer-3", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(claim3.is_none());
    }

    #[tokio::test]
    async fn test_lease_expiry() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        store
            .inner
            .queue_manager
            .enqueue_item("test-queue", "item-1", 1)
            .unwrap();

        let claim = store
            .claim_next("test-queue", "consumer-1", Duration::from_millis(1))
            .await
            .unwrap();
        assert!(claim.is_some());

        std::thread::sleep(Duration::from_millis(10));

        store.inner.queue_manager.cleanup_expired_claims().unwrap();

        let claim2 = store
            .claim_next("test-queue", "consumer-2", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(claim2.is_some());
    }

    #[tokio::test]
    async fn startup_recovery_cleans_expired_claims_automatically() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            store
                .inner
                .queue_manager
                .enqueue_item("test-queue", "item-1", 1)
                .unwrap();

            let claim = store
                .claim_next("test-queue", "consumer-1", Duration::from_millis(1))
                .await
                .unwrap();
            assert!(claim.is_some(), "initial claim should succeed");
            std::thread::sleep(Duration::from_millis(10));
            store.persist().unwrap();
        }

        let store = FjallEventStore::new(&path).unwrap();
        let reclaimed = store
            .claim_next("test-queue", "consumer-2", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            reclaimed.is_some(),
            "startup recovery should clear expired claim metadata so item can be reclaimed"
        );
    }

    #[tokio::test]
    async fn startup_recovery_preserves_same_epoch_monotonic_claims_with_stale_wall_clock_expiry() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        let initial_store = FjallEventStore::new(&path).unwrap();
        initial_store
            .inner
            .queue_manager
            .enqueue_item("test-queue", "item-1", 1)
            .unwrap();

        let claim = initial_store
            .claim_next("test-queue", "consumer-1", Duration::from_secs(60))
            .await
            .unwrap()
            .expect("initial claim should succeed");

        let lease_key = format!("lease:{}", claim.claim_id.as_str());
        let mut persisted: QueueClaim = serde_json::from_slice(
            &initial_store
                .inner
                .queue_manager
                .queue
                .get(lease_key.as_bytes())
                .unwrap()
                .expect("lease payload must exist"),
        )
        .unwrap();
        persisted.expires_at = chrono::Utc::now() - chrono::Duration::hours(1);
        initial_store
            .inner
            .queue_manager
            .queue
            .insert(
                lease_key.as_bytes(),
                serde_json::to_vec(&persisted).unwrap(),
            )
            .unwrap();
        initial_store.persist().unwrap();

        let reopened_store = FjallEventStore::new(&path).unwrap();
        let reclaimed = reopened_store
            .claim_next("test-queue", "consumer-2", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            reclaimed.is_none(),
            "startup recovery should not revoke same-epoch monotonic leases solely because wall-clock expiry is stale"
        );
    }

    #[tokio::test]
    async fn startup_recovery_tolerates_malformed_lease_payloads() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            store
                .inner
                .queue_manager
                .enqueue_item("test-queue", "item-1", 1)
                .unwrap();

            let claim = store
                .claim_next("test-queue", "consumer-1", Duration::from_millis(1))
                .await
                .unwrap();
            assert!(claim.is_some(), "initial claim should succeed");
            std::thread::sleep(Duration::from_millis(10));

            store
                .inner
                .queue_manager
                .queue
                .insert(b"lease:corrupt", b"{not-json")
                .unwrap();
            store.persist().unwrap();
        }

        let store = FjallEventStore::new(&path).expect(
            "startup recovery should skip malformed lease payloads instead of failing open",
        );
        let reclaimed = store
            .claim_next("test-queue", "consumer-2", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            reclaimed.is_some(),
            "startup recovery should still clean valid expired claims even when malformed leases exist"
        );
    }

    #[tokio::test]
    async fn startup_recovery_cleans_orphaned_task_and_prepared_merge_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();

            store
                .append(Event {
                    kind: EventKind::TaskCreated {
                        task_id: "T-orphan".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-orphan".into(),
                })
                .await
                .unwrap();
            store
                .append(Event {
                    kind: EventKind::TaskAssigned {
                        task_id: "T-orphan".into(),
                        agent_id: "worker-1".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-orphan".into(),
                })
                .await
                .unwrap();

            let orphan_view = TaskView {
                task_id: "T-orphan".into(),
                status: TaskStatus::InProgress,
                assignee: Some("worker-1".into()),
                session_id: None,
                branch: None,
                review_rounds: 0,
                last_event_id: 2,
                updated_at: chrono::Utc::now(),
            };
            store
                .inner
                .view_manager
                .views
                .insert(
                    task_view_key("T-orphan"),
                    serde_json::to_vec(&orphan_view).unwrap(),
                )
                .unwrap();

            store
                .append(Event {
                    kind: EventKind::TaskCreated {
                        task_id: "T-prepared".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-prepared".into(),
                })
                .await
                .unwrap();
            store
                .append(Event {
                    kind: EventKind::MergePrepared {
                        task_id: "T-prepared".into(),
                        branch: "feature/t-prepared".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-prepared".into(),
                })
                .await
                .unwrap();

            let prepared_view = TaskView {
                task_id: "T-prepared".into(),
                status: TaskStatus::Approved,
                assignee: None,
                session_id: None,
                branch: Some("feature/t-prepared".into()),
                review_rounds: 0,
                last_event_id: 4,
                updated_at: chrono::Utc::now(),
            };
            store
                .inner
                .view_manager
                .views
                .insert(
                    task_view_key("T-prepared"),
                    serde_json::to_vec(&prepared_view).unwrap(),
                )
                .unwrap();

            store
                .inner
                .meta
                .insert(KEY_META_VIEWS_LAST_EVENT_ID, 4u64.to_be_bytes())
                .unwrap();

            store.persist().unwrap();
        }

        let store = FjallEventStore::new(&path).unwrap();
        let orphan_view = store
            .inner
            .view_manager
            .get_task_view("T-orphan")
            .unwrap()
            .expect("orphan task view should still exist");
        assert_eq!(orphan_view.status, TaskStatus::Pending);
        assert!(orphan_view.assignee.is_none());
        assert!(orphan_view.session_id.is_none());

        let prepared_view = store
            .inner
            .view_manager
            .get_task_view("T-prepared")
            .unwrap()
            .expect("prepared task view should still exist");
        assert!(
            prepared_view.branch.is_none(),
            "startup recovery should clear prepared merge branch state"
        );
    }

    #[tokio::test]
    async fn test_concurrent_appends() {
        use std::thread;

        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let store = FjallEventStore::new(&path).unwrap();

        let mut handles = vec![];
        for i in 0..10 {
            let store = store.clone();
            let handle = thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    for j in 0..100 {
                        let event = Event {
                            kind: EventKind::TaskCreated {
                                task_id: format!("T{}-{}", i, j),
                            },
                            timestamp: chrono::Utc::now(),
                            aggregate_id: format!("T{}-{}", i, j),
                        };
                        let _ = store.append(event).await.unwrap();
                    }
                });
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let events = store.stream(None, 2000).await.unwrap();
        assert_eq!(events.len(), 1000);
    }

    #[tokio::test]
    async fn test_append_and_enqueue_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();
        let event = Event {
            kind: EventKind::ReviewRequested {
                task_id: "T001".into(),
                review_id: "R001".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "R001".into(),
        };

        let first_id = store
            .append_and_enqueue(
                event.clone(),
                "review:high",
                "R001",
                Some("review_requested:R001"),
            )
            .await
            .unwrap();
        let second_id = store
            .append_and_enqueue(event, "review:high", "R001", Some("review_requested:R001"))
            .await
            .unwrap();

        assert_eq!(first_id, second_id, "idempotent retry must reuse event id");

        let claim = store
            .claim_next("review:high", "consumer-1", Duration::from_secs(60))
            .await
            .unwrap()
            .expect("review should be claimable");
        assert_eq!(claim.item_id, "R001");

        store.ack_claim(&claim.claim_id).await.unwrap();

        let duplicate = store
            .claim_next("review:high", "consumer-2", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            duplicate.is_none(),
            "idempotent retry must not leave a duplicate queue record"
        );

        let events = store.stream(None, 10).await.unwrap();
        assert_eq!(
            events.len(),
            1,
            "idempotent retry must not duplicate events"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_concurrent_review_enqueues_are_unique_and_claimable() {
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let store = Arc::new(FjallEventStore::new(dir.path()).unwrap());
        let total = 128usize;

        let mut handles = Vec::with_capacity(total);
        for i in 0..total {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let review_id = format!("R{i:03}");
                let event = Event {
                    kind: EventKind::ReviewRequested {
                        task_id: format!("T{i:03}"),
                        review_id: review_id.clone(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: review_id.clone(),
                };
                let idempotency_key = format!("review_requested:{review_id}");
                store
                    .append_and_enqueue(event, "review:high", &review_id, Some(&idempotency_key))
                    .await
                    .unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let mut claimed = HashSet::new();
        while let Some(claim) = store
            .claim_next("review:high", "consumer", Duration::from_secs(60))
            .await
            .unwrap()
        {
            assert!(
                claimed.insert(claim.item_id.clone()),
                "review {} was claimable more than once",
                claim.item_id
            );
            store.ack_claim(&claim.claim_id).await.unwrap();
        }

        assert_eq!(
            claimed.len(),
            total,
            "all concurrently enqueued reviews must remain claimable"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_single_review_item_has_single_claim_winner_under_contention() {
        use std::sync::Arc;
        use tokio::sync::Barrier;

        let dir = tempdir().unwrap();
        let store = Arc::new(FjallEventStore::new(dir.path()).unwrap());
        store
            .append_and_enqueue(
                Event {
                    kind: EventKind::ReviewRequested {
                        task_id: "T001".into(),
                        review_id: "R001".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R001".into(),
                },
                "review:high",
                "R001",
                Some("review_requested:R001"),
            )
            .await
            .unwrap();

        let contenders = 24usize;
        let barrier = Arc::new(Barrier::new(contenders));
        let mut handles = Vec::with_capacity(contenders);

        for consumer_idx in 0..contenders {
            let barrier = Arc::clone(&barrier);
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .claim_next(
                        "review:high",
                        &format!("consumer-{consumer_idx}"),
                        Duration::from_secs(60),
                    )
                    .await
                    .unwrap()
            }));
        }

        let mut winners = Vec::new();
        for handle in handles {
            if let Some(claim) = handle.await.unwrap() {
                winners.push(claim);
            }
        }

        assert_eq!(
            winners.len(),
            1,
            "exactly one concurrent claimer should win a single review item"
        );
        assert_eq!(winners[0].item_id, "R001");

        let late_claim = store
            .claim_next("review:high", "late-consumer", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            late_claim.is_none(),
            "once one contender wins, the same review item must not be claimable again until ack/expiry"
        );
    }

    #[tokio::test]
    async fn test_concurrent_append_and_enqueue_preserve_seq_across_restart() {
        use std::thread;

        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let store = FjallEventStore::new(&path).unwrap();
        let append_threads = 4usize;
        let append_enqueue_threads = 4usize;
        let events_per_thread = 25usize;

        let mut handles = Vec::new();
        for i in 0..append_threads {
            let store = store.clone();
            handles.push(thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    for j in 0..events_per_thread {
                        let task_id = format!("A{i}-{j}");
                        store
                            .append(Event {
                                kind: EventKind::TaskCreated {
                                    task_id: task_id.clone(),
                                },
                                timestamp: chrono::Utc::now(),
                                aggregate_id: task_id,
                            })
                            .await
                            .unwrap();
                    }
                });
            }));
        }

        for i in 0..append_enqueue_threads {
            let store = store.clone();
            handles.push(thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    for j in 0..events_per_thread {
                        let review_id = format!("R{i}-{j}");
                        store
                            .append_and_enqueue(
                                Event {
                                    kind: EventKind::ReviewRequested {
                                        task_id: format!("T{i}-{j}"),
                                        review_id: review_id.clone(),
                                    },
                                    timestamp: chrono::Utc::now(),
                                    aggregate_id: review_id.clone(),
                                },
                                "review:high",
                                &review_id,
                                Some(&format!("review_requested:{review_id}")),
                            )
                            .await
                            .unwrap();
                    }
                });
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        store.persist().unwrap();
        drop(store);

        let reopened = FjallEventStore::new(&path).unwrap();
        let expected = (append_threads + append_enqueue_threads) * events_per_thread;
        let events = reopened.stream(None, expected + 10).await.unwrap();
        assert_eq!(events.len(), expected);

        for (index, (_, event_id)) in events.iter().enumerate() {
            assert_eq!(
                event_id.as_u64(),
                (index + 1) as u64,
                "event ids should remain contiguous across restart"
            );
        }

        let next_id = reopened
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "after-restart".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "after-restart".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_id.as_u64(),
            (expected + 1) as u64,
            "restart must continue from the highest persisted seq"
        );
    }

    #[tokio::test]
    async fn startup_rebuilds_task_and_review_views_from_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let events = vec![
                Event {
                    kind: EventKind::TaskCreated {
                        task_id: "T-rebuild".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-rebuild".into(),
                },
                Event {
                    kind: EventKind::TaskAssigned {
                        task_id: "T-rebuild".into(),
                        agent_id: "agent-1".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-rebuild".into(),
                },
                Event {
                    kind: EventKind::TaskCompleted {
                        task_id: "T-rebuild".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-rebuild".into(),
                },
                Event {
                    kind: EventKind::ReviewRequested {
                        task_id: "T-rebuild".into(),
                        review_id: "R-rebuild".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R-rebuild".into(),
                },
                Event {
                    kind: EventKind::ReviewScoreReceived {
                        review_id: "R-rebuild".into(),
                        reviewer_id: "reviewer-a".into(),
                        score: 8,
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R-rebuild".into(),
                },
                Event {
                    kind: EventKind::ReviewApproved {
                        review_id: "R-rebuild".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R-rebuild".into(),
                },
                Event {
                    kind: EventKind::MergePrepared {
                        task_id: "T-rebuild".into(),
                        branch: "feature/rebuild".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-rebuild".into(),
                },
            ];

            for event in events {
                store.append(event).await.unwrap();
            }

            // Poison existing views to verify startup rebuild can recover from corruption.
            store
                .inner
                .view_manager
                .views
                .insert(task_view_key("T-rebuild"), b"{not-json")
                .unwrap();
            store
                .inner
                .view_manager
                .views
                .insert(review_view_key("R-rebuild"), b"{not-json")
                .unwrap();
            store.persist().unwrap();
        }

        let store = FjallEventStore::new(&path).unwrap();
        let task_view = store
            .inner
            .view_manager
            .get_task_view("T-rebuild")
            .unwrap()
            .expect("task view should exist after rebuild");
        assert_eq!(task_view.status, TaskStatus::Approved);
        assert_eq!(task_view.assignee, Some("agent-1".to_string()));
        assert_eq!(task_view.review_rounds, 1);
        assert_eq!(
            task_view.branch, None,
            "startup recovery should clear prepared-merge branch state without committed/aborted merge"
        );

        let review_view = store
            .inner
            .view_manager
            .get_review_view("R-rebuild")
            .unwrap()
            .expect("review view should exist after rebuild");
        assert_eq!(review_view.status, ReviewStatus::Completed);
        assert_eq!(review_view.scores, vec![("reviewer-a".to_string(), 8)]);
        assert_eq!(review_view.round, 1);
    }

    #[tokio::test]
    async fn startup_rebuild_sets_task_status_from_review_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let events = vec![
                Event {
                    kind: EventKind::TaskCreated {
                        task_id: "T-rejected".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-rejected".into(),
                },
                Event {
                    kind: EventKind::TaskAssigned {
                        task_id: "T-rejected".into(),
                        agent_id: "agent-1".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-rejected".into(),
                },
                Event {
                    kind: EventKind::TaskCompleted {
                        task_id: "T-rejected".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-rejected".into(),
                },
                Event {
                    kind: EventKind::ReviewRequested {
                        task_id: "T-rejected".into(),
                        review_id: "R-rejected".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R-rejected".into(),
                },
                Event {
                    kind: EventKind::ReviewRejected {
                        review_id: "R-rejected".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R-rejected".into(),
                },
            ];

            for event in events {
                store.append(event).await.unwrap();
            }

            store
                .inner
                .view_manager
                .views
                .insert(task_view_key("T-rejected"), b"{not-json")
                .unwrap();
            store
                .inner
                .view_manager
                .views
                .insert(review_view_key("R-rejected"), b"{not-json")
                .unwrap();
            store.persist().unwrap();
        }

        let store = FjallEventStore::new(&path).unwrap();
        let task_view = store
            .inner
            .view_manager
            .get_task_view("T-rejected")
            .unwrap()
            .expect("task view should exist after rebuild");
        assert_eq!(task_view.status, TaskStatus::ChangesRequested);
    }

    #[tokio::test]
    async fn startup_rebuild_sets_task_status_from_review_changes_requested() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let events = vec![
                Event {
                    kind: EventKind::TaskCreated {
                        task_id: "T-changes".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-changes".into(),
                },
                Event {
                    kind: EventKind::TaskAssigned {
                        task_id: "T-changes".into(),
                        agent_id: "agent-1".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-changes".into(),
                },
                Event {
                    kind: EventKind::TaskCompleted {
                        task_id: "T-changes".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-changes".into(),
                },
                Event {
                    kind: EventKind::ReviewRequested {
                        task_id: "T-changes".into(),
                        review_id: "R-changes".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R-changes".into(),
                },
                Event {
                    kind: EventKind::ReviewChangesRequested {
                        review_id: "R-changes".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "R-changes".into(),
                },
            ];

            for event in events {
                store.append(event).await.unwrap();
            }

            store
                .inner
                .view_manager
                .views
                .insert(task_view_key("T-changes"), b"{not-json")
                .unwrap();
            store
                .inner
                .view_manager
                .views
                .insert(review_view_key("R-changes"), b"{not-json")
                .unwrap();
            store.persist().unwrap();
        }

        let store = FjallEventStore::new(&path).unwrap();
        let task_view = store
            .inner
            .view_manager
            .get_task_view("T-changes")
            .unwrap()
            .expect("task view should exist after rebuild");
        assert_eq!(task_view.status, TaskStatus::ChangesRequested);
    }

    #[tokio::test]
    async fn startup_skips_view_rebuild_when_views_watermark_matches_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let ids = store
                .append_atomic(
                    vec![Event {
                        kind: EventKind::TaskCreated {
                            task_id: "T-watermark".into(),
                        },
                        timestamp: chrono::Utc::now(),
                        aggregate_id: "T-watermark".into(),
                    }],
                    vec![
                        ViewUpdate {
                            view_type: ViewType::Task,
                            key: "T-watermark".into(),
                            operation: ViewOperation::Set {
                                field: "status".to_string(),
                                value: serde_json::to_string(&TaskStatus::InProgress).unwrap(),
                            },
                        },
                        ViewUpdate {
                            view_type: ViewType::Task,
                            key: "T-watermark".into(),
                            operation: ViewOperation::Set {
                                field: "session_id".to_string(),
                                value: "sess-1".to_string(),
                            },
                        },
                    ],
                )
                .await
                .unwrap();

            assert_eq!(ids, vec![EventId::new(1)]);
            let watermark = store
                .inner
                .meta
                .get(KEY_META_VIEWS_LAST_EVENT_ID)
                .unwrap()
                .unwrap();
            assert_eq!(
                u64::from_be_bytes(watermark.as_ref().try_into().unwrap()),
                1
            );
        }

        let reopened = FjallEventStore::new(&path).unwrap();
        let task_view = reopened
            .inner
            .view_manager
            .get_task_view("T-watermark")
            .unwrap()
            .expect("task view should exist after startup");
        assert_eq!(task_view.status, TaskStatus::InProgress);
        let reopened_watermark = reopened
            .inner
            .meta
            .get(KEY_META_VIEWS_LAST_EVENT_ID)
            .unwrap()
            .unwrap();
        assert_eq!(
            u64::from_be_bytes(reopened_watermark.as_ref().try_into().unwrap()),
            1
        );
    }

    #[tokio::test]
    async fn startup_rebuilds_views_if_append_atomic_has_no_view_updates() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let ids = store
                .append_atomic(
                    vec![
                        Event {
                            kind: EventKind::TaskCreated {
                                task_id: "T-watermark-no-view".into(),
                            },
                            timestamp: chrono::Utc::now(),
                            aggregate_id: "T-watermark-no-view".into(),
                        },
                        Event {
                            kind: EventKind::TaskAssigned {
                                task_id: "T-watermark-no-view".into(),
                                agent_id: "agent-1".into(),
                            },
                            timestamp: chrono::Utc::now(),
                            aggregate_id: "T-watermark-no-view".into(),
                        },
                    ],
                    Vec::new(),
                )
                .await
                .unwrap();
            assert_eq!(ids, vec![EventId::new(1), EventId::new(2)]);
            assert!(
                store
                    .inner
                    .meta
                    .get(KEY_META_VIEWS_LAST_EVENT_ID)
                    .unwrap()
                    .is_none(),
                "append_atomic without view updates should not advance the views watermark",
            );
        }

        let reopened = FjallEventStore::new(&path).unwrap();
        let task_view = reopened
            .inner
            .view_manager
            .get_task_view("T-watermark-no-view")
            .unwrap()
            .expect("task view should exist after rebuild");
        assert_eq!(task_view.status, TaskStatus::Assigned);
        let reopened_watermark = reopened
            .inner
            .meta
            .get(KEY_META_VIEWS_LAST_EVENT_ID)
            .unwrap()
            .unwrap();
        assert_eq!(
            u64::from_be_bytes(reopened_watermark.as_ref().try_into().unwrap()),
            2
        );
    }

    fn crash_event(task_id: &str) -> Event {
        Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.to_string(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: task_id.to_string(),
        }
    }

    fn inject_durable_event_without_seq_commit(
        store: &FjallEventStore,
        event: &Event,
    ) -> Result<EventId, StoreError> {
        let event_id = store.next_event_id();
        let envelope = EventEnvelope {
            event: event.clone(),
            event_id,
            correlation_id: None,
            causation_id: None,
            idempotency_key: None,
        };
        let key = log_key(event_id.as_u64());
        let value = FjallEventStore::serialize_event(&envelope)?;
        store.inner.events.insert(&key, &value)?;
        store.inner.keyspace.persist(PersistMode::SyncAll)?;
        Ok(event_id)
    }

    #[tokio::test]
    async fn crash_window_recovery_preserves_only_precrash_durable_prefix() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let _id1 = store.append(crash_event("T-TEAR-1")).await.unwrap();
            store.persist().unwrap();

            let _id2 = store.append(crash_event("T-TEAR-2")).await.unwrap();
            store.persist().unwrap();

            // Stop before the third event is even attempted. This verifies that
            // recovery preserves the prefix of events that was definitely
            // durable before the simulated crash.
        }

        let store = FjallEventStore::new(&path).unwrap();
        let events = store.stream(None, 100).await.unwrap();

        assert_eq!(
            events.len(),
            2,
            "After persisting 2 of 3 events before the simulated crash, exactly 2 should survive"
        );
        assert_eq!(
            events[0].0.kind,
            EventKind::TaskCreated {
                task_id: "T-TEAR-1".to_string()
            }
        );
        assert_eq!(
            events[1].0.kind,
            EventKind::TaskCreated {
                task_id: "T-TEAR-2".to_string()
            }
        );
    }

    #[tokio::test]
    async fn crash_window_no_eventid_reuse_after_recovery() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        let persisted_ids = {
            let store = FjallEventStore::new(&path).unwrap();
            let id1 = store.append(crash_event("T-REUSE-1")).await.unwrap();
            let id2 = store.append(crash_event("T-REUSE-2")).await.unwrap();
            store.persist().unwrap();
            vec![id1, id2]
        };

        let orphaned_id = {
            let store = FjallEventStore::new(&path).unwrap();
            inject_durable_event_without_seq_commit(&store, &crash_event("T-REUSE-ORPHAN")).unwrap()
        };

        let mut all_pre_crash_ids: HashSet<EventId> = persisted_ids.iter().copied().collect();
        all_pre_crash_ids.insert(orphaned_id);

        let store = FjallEventStore::new(&path).unwrap();
        let orphan_events = store
            .query(EventFilter::new().aggregate("T-REUSE-ORPHAN"))
            .await
            .unwrap();
        assert_eq!(
            orphan_events.len(),
            1,
            "The injected crash-window event should be durably visible after reopen"
        );

        let id4 = store.append(crash_event("T-REUSE-4")).await.unwrap();
        let id5 = store.append(crash_event("T-REUSE-5")).await.unwrap();

        for new_id in [id4, id5] {
            assert!(
                !all_pre_crash_ids.contains(&new_id),
                "Post-recovery EventId ({}) must not collide with any pre-crash ID {:?}",
                new_id,
                all_pre_crash_ids,
            );
        }
    }

    #[tokio::test]
    async fn crash_window_all_eventids_globally_unique() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        let batch1_ids = {
            let store = FjallEventStore::new(&path).unwrap();
            let events: Vec<Event> = (0..3)
                .map(|i| crash_event(&format!("T-UNIQ-{}", i)))
                .collect();
            let ids = store.append_atomic(events, Vec::new()).await.unwrap();
            store.persist().unwrap();
            ids
        };

        let orphan_id = {
            let store = FjallEventStore::new(&path).unwrap();
            inject_durable_event_without_seq_commit(&store, &crash_event("T-UNIQ-ORPHAN")).unwrap()
        };

        let store = FjallEventStore::new(&path).unwrap();
        let batch2_ids = store
            .append_atomic(
                (3..6)
                    .map(|i| Event {
                        kind: EventKind::TaskAssigned {
                            task_id: format!("T-UNIQ-{}", i),
                            agent_id: "worker-1".to_string(),
                        },
                        timestamp: chrono::Utc::now(),
                        aggregate_id: format!("T-UNIQ-{}", i),
                    })
                    .collect(),
                Vec::new(),
            )
            .await
            .unwrap();

        let mut all_ids = batch1_ids.clone();
        all_ids.push(orphan_id);
        all_ids.extend(batch2_ids.iter().copied());

        let unique: HashSet<EventId> = all_ids.iter().copied().collect();
        assert_eq!(
            all_ids.len(),
            unique.len(),
            "All assigned EventIds must remain globally unique across recovery boundaries: {:?}",
            all_ids,
        );
    }

    #[tokio::test]
    async fn crash_window_surviving_events_have_consistent_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let _id1 = store.append(crash_event("T-CON-1")).await.unwrap();
            let _id2 = store.append(crash_event("T-CON-2")).await.unwrap();
            store.persist().unwrap();
        }

        {
            let store = FjallEventStore::new(&path).unwrap();
            let orphan_id =
                inject_durable_event_without_seq_commit(&store, &crash_event("T-CON-ORPHAN"))
                    .unwrap();
            assert_eq!(
                orphan_id.as_u64(),
                3,
                "Expected deterministic crash-window ID"
            );
        }

        let store = FjallEventStore::new(&path).unwrap();
        let new_id = store.append(crash_event("T-CON-4")).await.unwrap();
        assert_eq!(
            new_id.as_u64(),
            4,
            "Recovery should resume after the highest durable log entry, not the stale seq key"
        );

        let events = store.query(EventFilter::new()).await.unwrap();
        assert_eq!(
            events.len(),
            4,
            "All four durable events should remain queryable"
        );

        let ids: Vec<EventId> = store
            .stream(None, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        assert_eq!(
            ids,
            vec![
                EventId::new(1),
                EventId::new(2),
                EventId::new(3),
                EventId::new(4)
            ],
            "Recovered stream should expose a strictly monotonic sequence without reuse",
        );
    }

    #[tokio::test]
    async fn append_atomic_does_not_commit_events_or_meta_when_view_staging_fails() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();
        let task_id = "T-APPEND-ATOMIC-FAIL";
        let seq_before = store.get_current_seq();

        store
            .inner
            .view_manager
            .views
            .insert(task_view_key(task_id), b"{not-json")
            .unwrap();

        let result = store
            .append_atomic(
                vec![crash_event(task_id)],
                vec![ViewUpdate {
                    view_type: ViewType::Task,
                    key: task_id.to_string(),
                    operation: ViewOperation::Set {
                        field: "status".to_string(),
                        value: "Completed".to_string(),
                    },
                }],
            )
            .await;
        assert!(
            result.is_err(),
            "view decoding failure should fail append_atomic"
        );
        assert_eq!(
            store.get_current_seq(),
            seq_before,
            "append_atomic staging failure must restore in-memory seq to avoid EventId gaps"
        );

        let events = store.stream(None, 10).await.unwrap();
        assert!(
            events.is_empty(),
            "append_atomic must not expose partially committed events when view staging fails"
        );
        assert!(
            store.inner.meta.get(KEY_META_SEQ).unwrap().is_none(),
            "append_atomic must not advance durable seq metadata when view staging fails"
        );

        let next_id = store
            .append(crash_event("T-APPEND-ATOMIC-NEXT"))
            .await
            .unwrap();
        assert_eq!(
            next_id.as_u64(),
            seq_before + 1,
            "first successful append after failed atomic staging should use the next contiguous EventId"
        );
        let stream_ids: Vec<EventId> = store
            .stream(None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        assert_eq!(
            stream_ids,
            vec![EventId::new(seq_before + 1)],
            "stream should remain gap-free after failed append_atomic recovery path"
        );
    }

    #[tokio::test]
    async fn append_writes_secondary_indexes_and_queries_use_them() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        let _ = store
            .append(Event {
                kind: EventKind::TaskAssigned {
                    task_id: "T-42".into(),
                    agent_id: "A-1".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "T-42".into(),
            })
            .await
            .unwrap();
        let _ = store
            .append(Event {
                kind: EventKind::ReviewRequested {
                    task_id: "T-42".into(),
                    review_id: "R-7".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "R-7".into(),
            })
            .await
            .unwrap();
        let _ = store
            .append(Event {
                kind: EventKind::AgentSpawned {
                    agent_id: "A-77".into(),
                    session_id: "S-1".into(),
                    role: "worker".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "S-1".into(),
            })
            .await
            .unwrap();

        let task_events = store.query(EventFilter::new().task("T-42")).await.unwrap();
        assert_eq!(
            task_events.len(),
            2,
            "task query should return both task-assigned and review-request events"
        );

        let mut review_filter = EventFilter::new();
        review_filter.review_id = Some("R-7".to_string());
        let review_events = store.query(review_filter).await.unwrap();
        assert_eq!(
            review_events.len(),
            1,
            "review query should return only review-requested event"
        );

        let agent_events = store.query(EventFilter::new().agent("A-1")).await.unwrap();
        assert_eq!(
            agent_events.len(),
            1,
            "agent query should return only task assignment events"
        );

        let mut task_index_entries = 0usize;
        for row in store.inner.events.prefix(&task_index_prefix("T-42")) {
            row.unwrap();
            task_index_entries += 1;
        }

        let mut review_index_entries = 0usize;
        for row in store.inner.events.prefix(&review_index_prefix("R-7")) {
            row.unwrap();
            review_index_entries += 1;
        }

        let mut agent_index_entries = 0usize;
        for row in store.inner.events.prefix(&agent_index_prefix("A-1")) {
            row.unwrap();
            agent_index_entries += 1;
        }

        assert_eq!(task_index_entries, 2, "both task events should be indexed");
        assert_eq!(
            review_index_entries, 1,
            "review event should be indexed once"
        );
        assert_eq!(agent_index_entries, 1, "agent event should be indexed once");
    }

    #[tokio::test]
    async fn query_task_includes_legacy_events_after_index_marker() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = FjallEventStore::new(&path).unwrap();
            let _ = inject_durable_event_without_seq_commit(
                &store,
                &Event {
                    kind: EventKind::TaskCreated {
                        task_id: "T-legacy".into(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: "T-legacy".into(),
                },
            )
            .unwrap();
            assert!(
                store
                    .inner
                    .meta
                    .get(KEY_META_INDEX_START_SEQ)
                    .unwrap()
                    .is_none(),
                "legacy-only events should not set index watermark"
            );
        }

        let store = FjallEventStore::new(&path).unwrap();
        let _ = store
            .append(Event {
                kind: EventKind::TaskAssigned {
                    task_id: "T-legacy".into(),
                    agent_id: "A-legacy".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "T-legacy".into(),
            })
            .await
            .unwrap();

        let watermark = store
            .inner
            .meta
            .get(KEY_META_INDEX_START_SEQ)
            .unwrap()
            .expect("index watermark should be set after first indexed event");
        assert_eq!(
            u64::from_be_bytes(watermark.as_ref().try_into().unwrap()),
            2
        );

        let events = store
            .query(EventFilter::new().task("T-legacy"))
            .await
            .unwrap();
        assert_eq!(
            events.len(),
            2,
            "legacy and indexed events should both be visible"
        );
        assert!(matches!(
            events[0].kind,
            brehon_types::EventKind::TaskCreated {
                task_id: ref id
            } if id == "T-legacy"
        ));
        assert!(matches!(
            events[1].kind,
            brehon_types::EventKind::TaskAssigned {
                task_id: ref id,
                ..
            } if id == "T-legacy"
        ));
    }

    #[tokio::test]
    async fn retain_events_archives_old_events() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        for i in 1..=5 {
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("T{}", i),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: format!("T{}", i),
            };
            store.append(event).await.unwrap();
        }

        assert_eq!(store.event_count().unwrap(), 5);
        let removed = store.retain_events(EventId::new(3)).await.unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.event_count().unwrap(), 3);
    }

    #[tokio::test]
    async fn expire_idempotency_keys_removes_stale_keys() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: "T1".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T1".into(),
        };
        store
            .append_and_enqueue(event, "q", "i1", Some("idem-1"))
            .await
            .unwrap();

        assert!(store
            .inner
            .meta
            .get(idempotency_key("idem-1"))
            .unwrap()
            .is_some());

        let removed = store
            .expire_idempotency_keys(Duration::from_secs(0))
            .await
            .unwrap();
        assert_eq!(removed, 1);

        assert!(store
            .inner
            .meta
            .get(idempotency_key("idem-1"))
            .unwrap()
            .is_none());
    }

    /// Regression guard for Fix A: the append path now fsyncs on the blocking
    /// pool. Confirm it still appends, preserves EventId monotonicity, and is
    /// queryable end-to-end after the spawn_blocking refactor.
    #[tokio::test]
    async fn append_via_spawn_blocking_round_trips_and_keeps_ids_monotonic() {
        let dir = tempdir().unwrap();
        let store = FjallEventStore::new(dir.path()).unwrap();

        let mut last = 0u64;
        for i in 0..32 {
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("T{i}"),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: format!("T{i}"),
            };
            let id = store.append(event).await.unwrap().as_u64();
            assert_eq!(id, last + 1, "event ids must be gap-free and monotonic");
            last = id;
        }

        let streamed = store.stream(None, 100).await.unwrap();
        assert_eq!(streamed.len(), 32);
        // Survives a restart: the blocking-pool fsync must still be durable.
        let store2 = FjallEventStore::new(dir.path()).unwrap();
        assert_eq!(store2.high_water_mark().await.unwrap().as_u64(), 32);
    }

    /// Fix A core property: the synchronous fsync must run on the blocking pool,
    /// not park a runtime worker. On a single-worker (current_thread) runtime, a
    /// blocking call directly on the worker would prevent any other task from
    /// making progress. We drive many appends concurrently with `join_all` and a
    /// cooperative heartbeat task; if appends blocked the lone worker, the
    /// heartbeat would be starved and the appends could deadlock. Completing
    /// proves the fsync was moved off the runtime worker.
    #[test]
    fn append_does_not_block_the_runtime_worker() {
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let store = FjallEventStore::new(&path).unwrap();
            let ticks = std::sync::Arc::new(AtomicU64::new(0));

            // Cooperative heartbeat: yields each iteration. It can only advance
            // if the single worker is not parked by a blocking fsync.
            let hb_ticks = ticks.clone();
            let heartbeat = tokio::spawn(async move {
                for _ in 0..1_000 {
                    hb_ticks.fetch_add(1, AtomicOrdering::SeqCst);
                    tokio::task::yield_now().await;
                }
            });

            let mut appends = Vec::new();
            for i in 0..64 {
                let store = store.clone();
                appends.push(tokio::spawn(async move {
                    let event = Event {
                        kind: EventKind::TaskCreated {
                            task_id: format!("HB{i}"),
                        },
                        timestamp: chrono::Utc::now(),
                        aggregate_id: format!("HB{i}"),
                    };
                    store.append(event).await.unwrap();
                }));
            }

            for handle in appends {
                handle.await.unwrap();
            }
            heartbeat.await.unwrap();

            // The heartbeat ran to completion alongside the blocking-pool fsyncs.
            assert_eq!(ticks.load(AtomicOrdering::SeqCst), 1_000);
            let events = store.stream(None, 256).await.unwrap();
            assert_eq!(events.len(), 64);
        });
    }
}
