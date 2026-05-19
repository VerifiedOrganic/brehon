//! In-memory EventStore implementation for testing.
//!
//! This implementation models atomic claims and replayable ordering,
//! NOT a naive Vec<Event> append.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use uuid::Uuid;

use brehon_ports::{EventStore, PortError};
use brehon_types::{
    ClaimId, Event, EventFilter, EventId, EventKind, QueueClaim, ViewOperation, ViewType,
    ViewUpdate,
};

/// Entry in the event log.
#[derive(Debug, Clone)]
struct LogEntry {
    event: Event,
    event_id: EventId,
}

/// Claim state for queue items.
#[derive(Debug, Clone)]
struct ClaimState {
    claim: QueueClaim,
}

/// In-memory event store with proper atomic claim semantics.
///
/// This is NOT a naive Vec<Event>. It properly models:
/// - Monotonic sequence numbers
/// - Atomic claims with durable lease semantics
/// - Idempotent appends
/// - Concurrent access safety
/// - Crash-recovery simulation via `mark_persisted` / `simulate_crash_recovery`
#[derive(Debug, Clone)]
pub struct InMemoryEventStore {
    inner: Arc<RwLock<StoreInner>>,
}

#[derive(Debug)]
struct StoreInner {
    events: Vec<LogEntry>,
    next_id: u64,
    persisted_seq: u64,
    high_water_mark: u64,
    idempotency_keys: HashMap<String, EventId>,
    queues: HashMap<String, Vec<QueueItem>>,
    queue_materialized: HashMap<String, EventId>,
    claims: HashMap<ClaimId, ClaimState>,
    views: HashMap<String, String>,
    persisted_views: HashMap<String, String>,
    persisted_queues: HashMap<String, Vec<QueueItem>>,
    persisted_queue_materialized: HashMap<String, EventId>,
}

#[derive(Debug, Clone)]
struct QueueItem {
    id: String,
    claimed: bool,
}

impl Default for StoreInner {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            next_id: 1,
            persisted_seq: 0,
            high_water_mark: 0,
            idempotency_keys: HashMap::new(),
            queues: HashMap::new(),
            queue_materialized: HashMap::new(),
            claims: HashMap::new(),
            views: HashMap::new(),
            persisted_views: HashMap::new(),
            persisted_queues: HashMap::new(),
            persisted_queue_materialized: HashMap::new(),
        }
    }
}

impl InMemoryEventStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StoreInner::default())),
        }
    }

    fn matches_filter(event: &Event, filter: &EventFilter) -> bool {
        if let Some(ref aggregate_id) = filter.aggregate_id {
            if event.aggregate_id != *aggregate_id {
                return false;
            }
        }

        if let Some(ref kinds) = filter.kinds {
            if !kinds.contains(&event.kind) {
                return false;
            }
        }

        if let Some(ref agent_id) = filter.agent_id {
            let matches = match &event.kind {
                EventKind::AgentSpawned { agent_id: a, .. } => a == agent_id,
                EventKind::AgentDied { agent_id: a, .. } => a == agent_id,
                EventKind::PromptSent { .. } => false,
                EventKind::ResponseReceived { .. } => false,
                EventKind::TaskAssigned { agent_id: a, .. } => a == agent_id,
                _ => false,
            };
            if !matches {
                return false;
            }
        }

        if let Some(ref task_id) = filter.task_id {
            let matches = match &event.kind {
                EventKind::TaskCreated { task_id: t } => t == task_id,
                EventKind::TaskAssigned { task_id: t, .. } => t == task_id,
                EventKind::TaskCompleted { task_id: t } => t == task_id,
                EventKind::RunCreated { task_id: t, .. }
                | EventKind::RunClaimed { task_id: t, .. }
                | EventKind::RunClaimRenewed { task_id: t, .. }
                | EventKind::RunStarted { task_id: t, .. }
                | EventKind::RunActivityObserved { task_id: t, .. }
                | EventKind::RunReleased { task_id: t, .. }
                | EventKind::RunRetryQueued { task_id: t, .. }
                | EventKind::RunCompleted { task_id: t, .. }
                | EventKind::RunFailed { task_id: t, .. }
                | EventKind::RunAbandoned { task_id: t, .. }
                | EventKind::StaleRunMutationRejected { task_id: t, .. } => t.as_str() == task_id,
                EventKind::ReviewRequested { task_id: t, .. } => t == task_id,
                EventKind::MergePrepared { task_id: t, .. } => t == task_id,
                EventKind::MergeCommitted { task_id: t } => t == task_id,
                EventKind::MergeAborted { task_id: t, .. } => t == task_id,
                _ => false,
            };
            if !matches {
                return false;
            }
        }

        if let Some(ref since) = filter.since {
            if event.timestamp < *since {
                return false;
            }
        }

        if let Some(ref until) = filter.until {
            if event.timestamp > *until {
                return false;
            }
        }

        true
    }

    pub fn len(&self) -> usize {
        self.inner.read().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().events.is_empty()
    }

    pub fn get_event(&self, id: EventId) -> Option<Event> {
        self.inner
            .read()
            .events
            .iter()
            .find(|e| e.event_id == id)
            .map(|e| e.event.clone())
    }

    pub fn push_event(&self, kind: EventKind, aggregate_id: impl Into<String>) -> EventId {
        let event = Event {
            kind,
            timestamp: Utc::now(),
            aggregate_id: aggregate_id.into(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(self.append(event)).unwrap()
    }

    pub fn all_events(&self) -> Vec<Event> {
        self.inner
            .read()
            .events
            .iter()
            .map(|e| e.event.clone())
            .collect()
    }

    pub fn enqueue(&self, queue: &str, item_id: &str, _data: &str) {
        let mut inner = self.inner.write();
        inner
            .queues
            .entry(queue.to_string())
            .or_default()
            .push(QueueItem {
                id: item_id.to_string(),
                claimed: false,
            });
    }

    pub fn queue_len(&self, queue: &str) -> usize {
        self.inner
            .read()
            .queues
            .get(queue)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    fn ensure_queue_item(inner: &mut StoreInner, queue: &str, item_id: &str) {
        let queue_items = inner.queues.entry(queue.to_string()).or_default();
        if queue_items.iter().any(|item| item.id == item_id) {
            return;
        }
        queue_items.push(QueueItem {
            id: item_id.to_string(),
            claimed: false,
        });
    }

    pub fn pending_count(&self, queue: &str) -> usize {
        self.inner
            .read()
            .queues
            .get(queue)
            .map(|q| q.iter().filter(|i| !i.claimed).count())
            .unwrap_or(0)
    }

    pub fn get_view(&self, view_type: &ViewType, key: &str) -> Option<String> {
        let storage_key = format!("{:?}:{}", view_type, key);
        self.inner.read().views.get(&storage_key).cloned()
    }

    pub fn mark_persisted(&self) {
        let mut inner = self.inner.write();
        inner.persisted_seq = inner.next_id.saturating_sub(1);
        inner.persisted_views = inner.views.clone();
        inner.persisted_queues = inner.queues.clone();
        inner.persisted_queue_materialized = inner.queue_materialized.clone();
    }

    pub fn simulate_crash_recovery(&self) -> Vec<EventId> {
        let mut inner = self.inner.write();
        let persisted_seq = inner.persisted_seq;

        let discarded: Vec<EventId> = inner
            .events
            .iter()
            .filter(|entry| entry.event_id.as_u64() > persisted_seq)
            .map(|entry| entry.event_id)
            .collect();

        inner
            .events
            .retain(|entry| entry.event_id.as_u64() <= persisted_seq);
        inner
            .idempotency_keys
            .retain(|_, event_id| event_id.as_u64() <= persisted_seq);
        inner.views = inner.persisted_views.clone();
        inner.queues = inner.persisted_queues.clone();
        inner.queue_materialized = inner.persisted_queue_materialized.clone();
        inner.claims.clear();
        inner.next_id = inner.high_water_mark + 1;
        inner.persisted_seq = inner.high_water_mark;

        discarded
    }

    pub fn persisted_seq(&self) -> u64 {
        self.inner.read().persisted_seq
    }

    pub fn current_seq(&self) -> u64 {
        self.inner.read().next_id
    }
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventStore for InMemoryEventStore {
    async fn append(&self, event: Event) -> Result<EventId, PortError> {
        let mut inner = self.inner.write();

        if let Some(ref key) = event.kind_as_idempotency_key() {
            if let Some(&existing_id) = inner.idempotency_keys.get(key) {
                return Ok(existing_id);
            }
        }

        let event_id = EventId::new(inner.next_id);
        inner.next_id += 1;
        inner.high_water_mark = std::cmp::max(inner.high_water_mark, event_id.as_u64());

        if let Some(key) = event.kind_as_idempotency_key() {
            inner.idempotency_keys.insert(key, event_id);
        }

        inner.events.push(LogEntry { event, event_id });

        Ok(event_id)
    }

    async fn append_atomic(
        &self,
        events: Vec<Event>,
        views: Vec<ViewUpdate>,
    ) -> Result<Vec<EventId>, PortError> {
        let mut inner = self.inner.write();
        let mut event_ids = Vec::with_capacity(events.len());

        for event in events {
            let event_id = EventId::new(inner.next_id);
            inner.next_id += 1;
            inner.high_water_mark = std::cmp::max(inner.high_water_mark, event_id.as_u64());

            if let Some(key) = event.kind_as_idempotency_key() {
                inner.idempotency_keys.insert(key, event_id);
            }

            inner.events.push(LogEntry { event, event_id });

            event_ids.push(event_id);
        }

        for update in views {
            let key = format!("{:?}:{}", update.view_type, update.key);
            match update.operation {
                ViewOperation::Set { value, .. } => {
                    inner.views.insert(key, value);
                }
                ViewOperation::Increment { amount, .. } => {
                    let current: i64 = inner
                        .views
                        .get(&key)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    inner.views.insert(key, (current + amount).to_string());
                }
                ViewOperation::Append { value, .. } => {
                    inner
                        .views
                        .entry(key)
                        .or_default()
                        .push_str(&format!(",{}", value));
                }
                ViewOperation::Remove { value, .. } => {
                    if let Some(existing) = inner.views.get_mut(&key) {
                        *existing = existing
                            .split(',')
                            .filter(|v| *v != value)
                            .collect::<Vec<_>>()
                            .join(",");
                    }
                }
            }
        }

        Ok(event_ids)
    }

    async fn append_and_enqueue(
        &self,
        event: Event,
        queue: &str,
        item_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<EventId, PortError> {
        let mut inner = self.inner.write();

        if let Some(key) = idempotency_key {
            if let Some(&existing_id) = inner.idempotency_keys.get(key) {
                if !inner.queue_materialized.contains_key(key) {
                    Self::ensure_queue_item(&mut inner, queue, item_id);
                    inner
                        .queue_materialized
                        .insert(key.to_string(), existing_id);
                }
                return Ok(existing_id);
            }
        }

        let event_id = EventId::new(inner.next_id);
        inner.next_id += 1;
        inner.high_water_mark = std::cmp::max(inner.high_water_mark, event_id.as_u64());

        if let Some(key) = idempotency_key {
            inner.idempotency_keys.insert(key.to_string(), event_id);
            inner.queue_materialized.insert(key.to_string(), event_id);
        }

        inner.events.push(LogEntry { event, event_id });
        Self::ensure_queue_item(&mut inner, queue, item_id);

        Ok(event_id)
    }

    async fn query(&self, filter: EventFilter) -> Result<Vec<Event>, PortError> {
        let inner = self.inner.read();

        let mut results: Vec<Event> = inner
            .events
            .iter()
            .filter(|entry| Self::matches_filter(&entry.event, &filter))
            .map(|entry| entry.event.clone())
            .collect();

        if let Some(limit) = filter.limit {
            results.truncate(limit);
        }

        Ok(results)
    }

    async fn stream(
        &self,
        since: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<(Event, EventId)>, PortError> {
        let inner = self.inner.read();

        let start = match since {
            Some(id) => inner
                .events
                .iter()
                .position(|e| e.event_id > id)
                .unwrap_or(inner.events.len()),
            None => 0,
        };

        Ok(inner
            .events
            .iter()
            .skip(start)
            .take(limit)
            .map(|e| (e.event.clone(), e.event_id))
            .collect())
    }

    async fn claim_next(
        &self,
        queue: &str,
        consumer: &str,
        lease_for: Duration,
    ) -> Result<Option<QueueClaim>, PortError> {
        let mut inner = self.inner.write();

        let now = Utc::now();

        let expired_ids: Vec<_> = inner
            .claims
            .iter()
            .filter(|(_, c)| c.claim.is_expired() && now > c.claim.expires_at)
            .map(|(id, c)| (id.clone(), c.claim.queue.clone(), c.claim.item_id.clone()))
            .collect();

        for (claim_id, queue_name, item_id) in expired_ids {
            inner.claims.remove(&claim_id);
            if let Some(queue_items) = inner.queues.get_mut(&queue_name) {
                for item in queue_items.iter_mut() {
                    if item.id == item_id {
                        item.claimed = false;
                    }
                }
            }
        }

        let queue_items = inner.queues.entry(queue.to_string()).or_default();

        let item_idx = queue_items.iter().position(|i| !i.claimed);

        if let Some(idx) = item_idx {
            let item = &mut queue_items[idx];
            item.claimed = true;

            let item_id = item.id.clone();
            let claim_id = ClaimId::new(Uuid::new_v4().to_string());
            let expires_at = Utc::now() + chrono::Duration::from_std(lease_for).unwrap();

            let claim = QueueClaim {
                claim_id: claim_id.clone(),
                queue: queue.to_string(),
                item_id: item_id.clone(),
                consumer: consumer.to_string(),
                expires_at,
                lease_epoch: None,
                lease_duration_ms: None,
                monotonic_deadline_ms: None,
            };

            inner.claims.insert(
                claim_id,
                ClaimState {
                    claim: claim.clone(),
                },
            );

            Ok(Some(claim))
        } else {
            Ok(None)
        }
    }

    async fn ack_claim(&self, claim_id: &ClaimId) -> Result<(), PortError> {
        let mut inner = self.inner.write();

        let claim_state = inner
            .claims
            .remove(claim_id)
            .ok_or_else(|| PortError::Storage("claim not found".into()))?;

        if claim_state.claim.is_expired() {
            return Err(PortError::Storage("claim expired".into()));
        }

        let item_id = claim_state.claim.item_id.clone();
        let queue_name = claim_state.claim.queue.clone();

        let queue = inner
            .queues
            .get_mut(&queue_name)
            .ok_or_else(|| PortError::Storage("queue not found".into()))?;

        queue.retain(|item| item.id != item_id);

        Ok(())
    }

    async fn renew_claim(&self, claim_id: &ClaimId, lease_for: Duration) -> Result<(), PortError> {
        let mut inner = self.inner.write();

        let claim_state = inner
            .claims
            .get_mut(claim_id)
            .ok_or_else(|| PortError::Storage("claim not found".into()))?;

        if claim_state.claim.is_expired() {
            return Err(PortError::Storage("claim expired".into()));
        }

        claim_state.claim.expires_at = Utc::now() + chrono::Duration::from_std(lease_for).unwrap();

        Ok(())
    }

    async fn high_water_mark(&self) -> Result<EventId, PortError> {
        let inner = self.inner.read();
        Ok(EventId::new(inner.next_id.saturating_sub(1)))
    }

    async fn retain_events(&self, before: EventId) -> Result<usize, PortError> {
        let mut inner = self.inner.write();
        let before_seq = before.as_u64();
        let before_len = inner.events.len();
        inner
            .events
            .retain(|entry| entry.event_id.as_u64() >= before_seq);
        inner
            .idempotency_keys
            .retain(|_, event_id| event_id.as_u64() >= before_seq);
        let removed = before_len - inner.events.len();
        Ok(removed)
    }

    async fn expire_idempotency_keys(&self, older_than: Duration) -> Result<usize, PortError> {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or_else(|_| chrono::Duration::MAX);
        let mut inner = self.inner.write();
        let before_len = inner.idempotency_keys.len();
        let event_timestamps: std::collections::HashMap<EventId, chrono::DateTime<chrono::Utc>> =
            inner
                .events
                .iter()
                .map(|e| (e.event_id, e.event.timestamp))
                .collect();
        inner.idempotency_keys.retain(|_, event_id| {
            event_timestamps
                .get(event_id)
                .map_or(false, |ts| *ts >= cutoff)
        });
        let removed = before_len - inner.idempotency_keys.len();
        Ok(removed)
    }
}

trait EventExt {
    fn kind_as_idempotency_key(&self) -> Option<String>;
}

impl EventExt for Event {
    fn kind_as_idempotency_key(&self) -> Option<String> {
        match &self.kind {
            EventKind::TaskCreated { task_id } => Some(format!("task_created:{}", task_id)),
            EventKind::ReviewRequested { review_id, .. } => {
                Some(format!("review_requested:{}", review_id))
            }
            EventKind::MergeCommitted { task_id } => Some(format!("merge_committed:{}", task_id)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_events_in_order() {
        let store = InMemoryEventStore::new();

        let e1 = store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T001".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".into(),
            })
            .await
            .unwrap();

        let e2 = store
            .append(Event {
                kind: EventKind::TaskAssigned {
                    task_id: "T001".into(),
                    agent_id: "agent-1".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".into(),
            })
            .await
            .unwrap();

        assert!(e1 < e2);
        assert_eq!(store.len(), 2);
    }

    #[tokio::test]
    async fn query_by_aggregate() {
        let store = InMemoryEventStore::new();

        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T001".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".into(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T002".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T002".into(),
            })
            .await
            .unwrap();

        let filter = EventFilter::new().aggregate("T001");
        let results = store.query(filter).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            &results[0].kind,
            EventKind::TaskCreated { task_id } if task_id == "T001"
        ));
    }

    #[tokio::test]
    async fn stream_since() {
        let store = InMemoryEventStore::new();

        let e1 = store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T001".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".into(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T002".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T002".into(),
            })
            .await
            .unwrap();

        let results = store.stream(Some(e1), 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].0.kind,
            EventKind::TaskCreated {
                task_id: "T002".into()
            }
        );
    }

    #[tokio::test]
    async fn claim_next_atomic() {
        let store = InMemoryEventStore::new();
        store.enqueue("review:high", "R001", "data");
        store.enqueue("review:high", "R002", "data2");

        let claim1 = store
            .claim_next("review:high", "reviewer-1", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(claim1.item_id, "R001");

        let claim2 = store
            .claim_next("review:high", "reviewer-2", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(claim2.item_id, "R002");

        let claim3 = store
            .claim_next("review:high", "reviewer-3", Duration::from_secs(60))
            .await
            .unwrap();

        assert!(claim3.is_none());
    }

    #[tokio::test]
    async fn claim_expiry_and_renew() {
        let store = InMemoryEventStore::new();
        store.enqueue("test", "T001", "data");

        let claim = store
            .claim_next("test", "consumer", Duration::from_millis(1))
            .await
            .unwrap()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        let renew_result = store
            .renew_claim(&claim.claim_id, Duration::from_secs(60))
            .await;
        assert!(renew_result.is_err());

        let new_claim = store
            .claim_next("test", "consumer2", Duration::from_secs(60))
            .await
            .unwrap();

        assert!(new_claim.is_some());
    }

    #[tokio::test]
    async fn ack_claim_removes_item() {
        let store = InMemoryEventStore::new();
        store.enqueue("test", "T001", "data");

        let claim = store
            .claim_next("test", "consumer", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(store.pending_count("test"), 0);

        store.ack_claim(&claim.claim_id).await.unwrap();

        assert_eq!(store.queue_len("test"), 0);
    }

    #[tokio::test]
    async fn concurrent_claims_no_double_claim() {
        let store = Arc::new(InMemoryEventStore::new());
        store.enqueue("test", "T001", "data");

        let mut handles = vec![];

        for i in 0..10 {
            let store = Arc::clone(&store);
            let handle = tokio::spawn(async move {
                store
                    .claim_next("test", &format!("consumer-{}", i), Duration::from_secs(60))
                    .await
                    .unwrap()
            });
            handles.push(handle);
        }

        let results: Vec<_> = futures::future::join_all(handles).await;

        let successful_claims: Vec<_> = results.into_iter().filter_map(|r| r.unwrap()).collect();

        assert_eq!(successful_claims.len(), 1);

        assert!(
            EventStore::claim_next(&*store, "test", "late", Duration::from_secs(60))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn append_and_enqueue_is_idempotent() {
        let store = InMemoryEventStore::new();
        let event = Event {
            kind: EventKind::ReviewRequested {
                task_id: "T001".into(),
                review_id: "R001".into(),
            },
            timestamp: Utc::now(),
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

        assert_eq!(first_id, second_id);
        assert_eq!(store.len(), 1);
        assert_eq!(store.queue_len("review:high"), 1);

        let claim = store
            .claim_next("review:high", "consumer-1", Duration::from_secs(60))
            .await
            .unwrap()
            .expect("review should be claimable");
        store.ack_claim(&claim.claim_id).await.unwrap();

        store
            .append_and_enqueue(
                Event {
                    kind: EventKind::ReviewRequested {
                        task_id: "T001".into(),
                        review_id: "R001".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "R001".into(),
                },
                "review:high",
                "R001",
                Some("review_requested:R001"),
            )
            .await
            .unwrap();

        let duplicate = store
            .claim_next("review:high", "consumer-2", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(duplicate.is_none());
    }
}
