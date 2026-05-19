//! Priority-lane queue operations.
//!
//! Handles atomic claims with durable lease semantics for the review queue.

use chrono::Utc;
use fjall::PartitionHandle;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::keys::*;
use crate::store::StoreError;
use brehon_types::{ClaimId, QueueClaim};

static CLAIM_LOCK_REGISTRY: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
static LEASE_CLOCK_REGISTRY: OnceLock<Mutex<HashMap<String, Weak<LeaseClock>>>> = OnceLock::new();

fn claim_lock_registry() -> &'static Mutex<HashMap<String, Weak<Mutex<()>>>> {
    CLAIM_LOCK_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lease_clock_registry() -> &'static Mutex<HashMap<String, Weak<LeaseClock>>> {
    LEASE_CLOCK_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prune_claim_lock_registry(registry: &mut HashMap<String, Weak<Mutex<()>>>) {
    registry.retain(|_, lock| lock.strong_count() > 0);
}

fn prune_lease_clock_registry(registry: &mut HashMap<String, Weak<LeaseClock>>) {
    registry.retain(|_, clock| clock.strong_count() > 0);
}

fn duration_ms_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn chrono_duration_from_std(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or_else(|_| chrono::Duration::seconds(60))
}

#[derive(Debug)]
struct LeaseClock {
    epoch: String,
    started_at: Instant,
}

impl LeaseClock {
    fn new() -> Self {
        Self {
            epoch: Uuid::new_v4().to_string(),
            started_at: Instant::now(),
        }
    }

    fn epoch(&self) -> &str {
        &self.epoch
    }

    fn elapsed_ms(&self) -> u64 {
        duration_ms_saturating(self.started_at.elapsed())
    }

    fn deadline_ms_from_now(&self, lease_for: Duration) -> u64 {
        self.elapsed_ms()
            .saturating_add(duration_ms_saturating(lease_for))
    }
}

fn shared_lease_clock(claim_lock_scope: &str) -> Arc<LeaseClock> {
    let registry = lease_clock_registry();
    let mut registry = registry.lock().expect("lease clock registry poisoned");
    prune_lease_clock_registry(&mut registry);

    if let Some(clock) = registry.get(claim_lock_scope).and_then(Weak::upgrade) {
        return clock;
    }

    let clock = Arc::new(LeaseClock::new());
    registry.insert(claim_lock_scope.to_owned(), Arc::downgrade(&clock));
    clock
}

#[cfg(test)]
type ClaimLockDropHookFn = Arc<dyn Fn() + Send + Sync + 'static>;

#[cfg(test)]
#[derive(Clone)]
struct ClaimLockDropHook {
    scope: String,
    hook: ClaimLockDropHookFn,
}

#[cfg(test)]
static CLAIM_LOCK_DROP_HOOK: OnceLock<Mutex<Option<ClaimLockDropHook>>> = OnceLock::new();

#[cfg(test)]
fn claim_lock_drop_hook() -> &'static Mutex<Option<ClaimLockDropHook>> {
    CLAIM_LOCK_DROP_HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn run_claim_lock_drop_hook(scope: &str) {
    let hook = {
        let mut hook = claim_lock_drop_hook()
            .lock()
            .expect("claim lock drop hook poisoned");

        match hook.as_ref() {
            Some(installed) if installed.scope == scope => hook.take().map(|hook| hook.hook),
            _ => None,
        }
    };

    if let Some(hook) = hook {
        hook();
    }
}

fn shared_claim_lock(claim_lock_scope: &str) -> Arc<Mutex<()>> {
    let registry = claim_lock_registry();
    let mut registry = registry.lock().expect("claim lock registry poisoned");
    prune_claim_lock_registry(&mut registry);

    if let Some(lock) = registry.get(claim_lock_scope).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(Mutex::new(()));
    registry.insert(claim_lock_scope.to_owned(), Arc::downgrade(&lock));
    lock
}

pub struct QueueManager {
    pub queue: PartitionHandle,
    // Phase 0 tradeoff: serialize claim/ack/renew/cleanup for one store path inside
    // this process. That keeps single-winner claims intact even if the same store is
    // reopened into another QueueManager, at the cost of cross-queue contention until
    // claim state can move to real storage-level CAS/transactions.
    claim_lock_scope: String,
    claim_lock: Arc<Mutex<()>>,
    lease_clock: Arc<LeaseClock>,
}

impl QueueManager {
    pub fn new(queue: PartitionHandle, claim_lock_scope: impl Into<String>) -> Self {
        let claim_lock_scope = claim_lock_scope.into();

        Self {
            queue,
            claim_lock_scope: claim_lock_scope.clone(),
            claim_lock: shared_claim_lock(&claim_lock_scope),
            lease_clock: shared_lease_clock(&claim_lock_scope),
        }
    }

    pub fn active_lease_epoch(&self) -> &str {
        self.lease_clock.epoch()
    }

    pub fn active_lease_elapsed_ms(&self) -> u64 {
        self.lease_clock.elapsed_ms()
    }

    pub fn claim_next(
        &self,
        queue_name: &str,
        consumer: &str,
        lease_for: Duration,
    ) -> Result<Option<QueueClaim>, StoreError> {
        let _guard = self.claim_lock.lock().expect("claim lock poisoned");
        let queue_prefix = queue_prefix(queue_name);
        let expires_at = Utc::now() + chrono_duration_from_std(lease_for);
        let lease_duration_ms = duration_ms_saturating(lease_for);
        let monotonic_deadline_ms = self.lease_clock.deadline_ms_from_now(lease_for);
        let lease_epoch = self.lease_clock.epoch().to_owned();

        let iter = self.queue.iter();

        for result in iter {
            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            if !key.starts_with(&queue_prefix) {
                continue;
            }

            let item_id_str = String::from_utf8_lossy(&value).to_string();

            let claimed_key = format!("claimed:{}:{}", queue_name, &item_id_str);

            if self.claim_marker_is_active(claimed_key.as_bytes(), queue_name, &item_id_str)? {
                continue;
            }

            let claim_id = ClaimId::new(Uuid::new_v4().to_string());

            let claim = QueueClaim {
                claim_id: claim_id.clone(),
                queue: queue_name.to_string(),
                item_id: item_id_str,
                consumer: consumer.to_string(),
                expires_at,
                lease_epoch: Some(lease_epoch.clone()),
                lease_duration_ms: Some(lease_duration_ms),
                monotonic_deadline_ms: Some(monotonic_deadline_ms),
            };

            let lease_key = lease_key(claim_id.as_str());
            let claim_bytes = serde_json::to_vec(&claim)?;

            self.queue.insert(&lease_key, &claim_bytes)?;
            self.queue
                .insert(claimed_key.as_bytes(), claim_id.as_str().as_bytes())?;

            return Ok(Some(claim));
        }

        Ok(None)
    }

    fn claim_marker_is_active(
        &self,
        claimed_key: &[u8],
        queue_name: &str,
        item_id: &str,
    ) -> Result<bool, StoreError> {
        let Some(claimed_value) = self.queue.get(claimed_key)? else {
            return Ok(false);
        };

        let claim_id = String::from_utf8_lossy(claimed_value.as_ref()).to_string();
        let lease_key = lease_key(&claim_id);

        let Some(lease_value) = self.queue.get(&lease_key)? else {
            tracing::warn!(
                claimed_key = %String::from_utf8_lossy(claimed_key),
                claim_id = %claim_id,
                "Removing stale claimed marker with missing lease"
            );
            self.queue.remove(claimed_key)?;
            return Ok(false);
        };

        let claim: QueueClaim = match serde_json::from_slice(&lease_value) {
            Ok(claim) => claim,
            Err(err) => {
                tracing::warn!(
                    claimed_key = %String::from_utf8_lossy(claimed_key),
                    claim_id = %claim_id,
                    error = %err,
                    "Removing stale claimed marker with malformed lease payload"
                );
                self.queue.remove(&lease_key)?;
                self.queue.remove(claimed_key)?;
                return Ok(false);
            }
        };

        if self.claim_is_expired(&claim) {
            self.queue.remove(&lease_key)?;
            self.queue.remove(claimed_key)?;
            return Ok(false);
        }

        if claim.queue != queue_name || claim.item_id != item_id {
            tracing::warn!(
                claimed_key = %String::from_utf8_lossy(claimed_key),
                claim_id = %claim_id,
                lease_queue = %claim.queue,
                lease_item = %claim.item_id,
                queue = %queue_name,
                item = %item_id,
                "Removing inconsistent claimed marker while preserving lease payload"
            );
            self.queue.remove(claimed_key)?;
            return Ok(false);
        }

        Ok(true)
    }

    pub fn ack_claim(&self, claim_id: &ClaimId) -> Result<(), StoreError> {
        let _guard = self.claim_lock.lock().expect("claim lock poisoned");
        let lease_key = lease_key(claim_id.as_str());

        let claim_bytes = self
            .queue
            .get(&lease_key)?
            .ok_or_else(|| StoreError::ClaimNotFound(claim_id.to_string()))?;

        let claim: QueueClaim = serde_json::from_slice(&claim_bytes)?;

        if self.claim_is_expired(&claim) {
            let claimed_key = format!("claimed:{}:{}", claim.queue, claim.item_id);
            self.queue.remove(&lease_key)?;
            self.queue.remove(claimed_key.as_bytes())?;
            return Err(StoreError::ClaimExpired(claim_id.to_string()));
        }

        let claimed_key = format!("claimed:{}:{}", claim.queue, claim.item_id);
        let queue_prefix = queue_prefix(&claim.queue);

        self.queue.remove(&lease_key)?;
        self.queue.remove(claimed_key.as_bytes())?;

        let iter = self.queue.iter();
        for result in iter {
            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            if !key.starts_with(&queue_prefix) {
                continue;
            }

            if String::from_utf8_lossy(&value) == claim.item_id {
                self.queue.remove(key)?;
                break;
            }
        }

        Ok(())
    }

    pub fn renew_claim(&self, claim_id: &ClaimId, lease_for: Duration) -> Result<(), StoreError> {
        let _guard = self.claim_lock.lock().expect("claim lock poisoned");
        let lease_key = lease_key(claim_id.as_str());

        let claim_bytes = self
            .queue
            .get(&lease_key)?
            .ok_or_else(|| StoreError::ClaimNotFound(claim_id.to_string()))?;

        let mut claim: QueueClaim = serde_json::from_slice(&claim_bytes)?;

        if self.claim_is_expired(&claim) {
            let claimed_key = format!("claimed:{}:{}", claim.queue, claim.item_id);
            self.queue.remove(&lease_key)?;
            self.queue.remove(claimed_key.as_bytes())?;
            return Err(StoreError::ClaimExpired(claim_id.to_string()));
        }

        claim.expires_at = Utc::now() + chrono_duration_from_std(lease_for);
        claim.lease_epoch = Some(self.lease_clock.epoch().to_owned());
        claim.lease_duration_ms = Some(duration_ms_saturating(lease_for));
        claim.monotonic_deadline_ms = Some(self.lease_clock.deadline_ms_from_now(lease_for));

        let new_bytes = serde_json::to_vec(&claim)?;
        self.queue.insert(&lease_key, &new_bytes)?;

        Ok(())
    }

    #[cfg(test)]
    pub fn enqueue_item(
        &self,
        queue_name: &str,
        item_id: &str,
        sequence: u64,
    ) -> Result<(), StoreError> {
        let key = queue_key(queue_name, sequence);
        self.queue.insert(&key, item_id.as_bytes())?;
        Ok(())
    }

    fn list_expired_claims(&self) -> Result<Vec<QueueClaim>, StoreError> {
        let mut expired = Vec::new();
        let prefix = lease_prefix();

        let iter = self.queue.iter();
        for result in iter {
            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;

            if !key.starts_with(&prefix) {
                continue;
            }

            let claim: QueueClaim = match serde_json::from_slice(&value) {
                Ok(claim) => claim,
                Err(err) => {
                    tracing::warn!(
                        key = %String::from_utf8_lossy(&key),
                        error = %err,
                        "Skipping malformed lease payload during expired-claim scan"
                    );
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
            if epoch != self.lease_clock.epoch() {
                return true;
            }

            if let Some(monotonic_deadline_ms) = claim.monotonic_deadline_ms {
                return self.lease_clock.elapsed_ms() >= monotonic_deadline_ms;
            }
        }

        claim.is_expired()
    }

    pub fn cleanup_expired_claims(&self) -> Result<usize, StoreError> {
        let _guard = self.claim_lock.lock().expect("claim lock poisoned");
        let expired = self.list_expired_claims()?;
        let count = expired.len();

        for claim in &expired {
            let lease_key = lease_key(claim.claim_id.as_str());
            let claimed_key = format!("claimed:{}:{}", claim.queue, claim.item_id);
            self.queue.remove(&lease_key)?;
            self.queue.remove(claimed_key.as_bytes())?;
        }

        Ok(count)
    }
}

impl Drop for QueueManager {
    fn drop(&mut self) {
        #[cfg(test)]
        run_claim_lock_drop_hook(&self.claim_lock_scope);

        let registry = claim_lock_registry();
        let mut registry = registry.lock().expect("claim lock registry poisoned");

        let should_remove = registry
            .get(&self.claim_lock_scope)
            .filter(|lock| lock.strong_count() == 1)
            .and_then(Weak::upgrade)
            .is_some_and(|lock| Arc::ptr_eq(&lock, &self.claim_lock));

        if should_remove {
            registry.remove(&self.claim_lock_scope);
        }

        prune_claim_lock_registry(&mut registry);

        let lease_registry = lease_clock_registry();
        let mut lease_registry = lease_registry
            .lock()
            .expect("lease clock registry poisoned");

        let should_remove_clock = lease_registry
            .get(&self.claim_lock_scope)
            .filter(|clock| clock.strong_count() == 1)
            .and_then(Weak::upgrade)
            .is_some_and(|clock| Arc::ptr_eq(&clock, &self.lease_clock));

        if should_remove_clock {
            lease_registry.remove(&self.claim_lock_scope);
        }

        prune_lease_clock_registry(&mut lease_registry);
    }
}

#[cfg(test)]
pub fn enqueue_item(
    queue: &PartitionHandle,
    queue_name: &str,
    item_id: &str,
    sequence: u64,
) -> Result<(), StoreError> {
    let key = queue_key(queue_name, sequence);
    queue.insert(&key, item_id.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fjall::{Config, PartitionCreateOptions};
    use std::sync::{Arc, Barrier as StdBarrier};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::Barrier as AsyncBarrier;

    fn install_claim_lock_drop_hook(scope: String, hook: ClaimLockDropHookFn) {
        *claim_lock_drop_hook()
            .lock()
            .expect("claim lock drop hook poisoned") = Some(ClaimLockDropHook { scope, hook });
    }

    fn claim_lock_registry_contains_scope(scope: &str) -> bool {
        claim_lock_registry()
            .lock()
            .expect("claim lock registry poisoned")
            .contains_key(scope)
    }

    #[test]
    fn test_queue_key_format() {
        let key = queue_key("review:high", 42);
        assert!(key.starts_with(b"queue:review:high:"));
    }

    #[test]
    fn test_lease_key_format() {
        let key = lease_key("claim-123");
        assert_eq!(key, b"lease:claim-123");
    }

    #[test]
    fn test_expired_claim_detection() {
        let claim = QueueClaim {
            claim_id: ClaimId::new("c1"),
            queue: "test".to_string(),
            item_id: "item-1".to_string(),
            consumer: "consumer-1".to_string(),
            expires_at: Utc::now() - chrono::Duration::seconds(1),
            lease_epoch: None,
            lease_duration_ms: None,
            monotonic_deadline_ms: None,
        };
        assert!(claim.is_expired());

        let valid_claim = QueueClaim {
            claim_id: ClaimId::new("c2"),
            queue: "test".to_string(),
            item_id: "item-2".to_string(),
            consumer: "consumer-1".to_string(),
            expires_at: Utc::now() + chrono::Duration::seconds(60),
            lease_epoch: None,
            lease_duration_ms: None,
            monotonic_deadline_ms: None,
        };
        assert!(!valid_claim.is_expired());
    }

    #[test]
    fn test_claim_next_reclaims_expired_lease_without_restart() {
        let dir = tempdir().unwrap();
        let keyspace = Config::new(dir.path()).open().unwrap();
        let queue = keyspace
            .open_partition("queue", PartitionCreateOptions::default())
            .unwrap();
        let manager = QueueManager::new(queue, dir.path().to_string_lossy().into_owned());

        manager.enqueue_item("review:high", "R001", 1).unwrap();

        let initial = manager
            .claim_next("review:high", "consumer-a", Duration::from_millis(1))
            .unwrap();
        assert!(initial.is_some(), "initial claim should succeed");

        std::thread::sleep(Duration::from_millis(10));

        let reclaimed = manager
            .claim_next("review:high", "consumer-b", Duration::from_secs(60))
            .unwrap();
        assert!(
            reclaimed.is_some(),
            "expired lease should become claimable without external cleanup or restart"
        );
        assert_eq!(reclaimed.unwrap().item_id, "R001");
    }

    #[test]
    fn test_inconsistent_claim_marker_does_not_delete_foreign_active_lease() {
        let dir = tempdir().unwrap();
        let keyspace = Config::new(dir.path()).open().unwrap();
        let queue = keyspace
            .open_partition("queue", PartitionCreateOptions::default())
            .unwrap();
        let manager = QueueManager::new(queue, dir.path().to_string_lossy().into_owned());

        manager.enqueue_item("review:high", "R001", 1).unwrap();
        manager.enqueue_item("review:high", "R002", 2).unwrap();

        let first_claim = manager
            .claim_next("review:high", "consumer-a", Duration::from_secs(60))
            .unwrap()
            .expect("first item should be claimable");

        let inconsistent_marker = b"claimed:review:high:R002";
        manager
            .queue
            .insert(
                inconsistent_marker,
                first_claim.claim_id.as_str().as_bytes(),
            )
            .unwrap();

        let second_claim = manager
            .claim_next("review:high", "consumer-b", Duration::from_secs(60))
            .unwrap()
            .expect("second item should be claimable after clearing inconsistent marker");
        assert_eq!(second_claim.item_id, "R002");

        manager
            .renew_claim(&first_claim.claim_id, Duration::from_secs(60))
            .expect("inconsistent marker handling must not revoke unrelated active lease");
    }

    #[test]
    fn test_same_epoch_claim_uses_monotonic_deadline_over_wall_clock_deadline() {
        let dir = tempdir().unwrap();
        let keyspace = Config::new(dir.path()).open().unwrap();
        let queue = keyspace
            .open_partition("queue", PartitionCreateOptions::default())
            .unwrap();
        let manager = QueueManager::new(queue, dir.path().to_string_lossy().into_owned());

        manager.enqueue_item("review:high", "R001", 1).unwrap();

        let first = manager
            .claim_next("review:high", "consumer-a", Duration::from_secs(60))
            .unwrap()
            .expect("initial claim should succeed");

        let lease_key = lease_key(first.claim_id.as_str());
        let mut persisted: QueueClaim = serde_json::from_slice(
            &manager
                .queue
                .get(&lease_key)
                .unwrap()
                .expect("lease payload must exist"),
        )
        .unwrap();
        persisted.expires_at = Utc::now() - chrono::Duration::hours(1);
        manager
            .queue
            .insert(&lease_key, &serde_json::to_vec(&persisted).unwrap())
            .unwrap();

        let second = manager
            .claim_next("review:high", "consumer-b", Duration::from_secs(60))
            .unwrap();
        assert!(
            second.is_none(),
            "same-epoch claim should remain active using monotonic deadline even if wall clock deadline is stale"
        );
    }

    #[test]
    fn test_restart_invalidates_prior_epoch_claim_without_wall_clock_dependency() {
        let dir = tempdir().unwrap();
        let keyspace = Config::new(dir.path()).open().unwrap();
        let queue = keyspace
            .open_partition("queue", PartitionCreateOptions::default())
            .unwrap();
        let scope = dir.path().to_string_lossy().into_owned();

        let first_manager = QueueManager::new(queue.clone(), scope.clone());
        first_manager
            .enqueue_item("review:high", "R001", 1)
            .unwrap();
        let first_claim = first_manager
            .claim_next("review:high", "consumer-a", Duration::from_secs(300))
            .unwrap()
            .expect("initial claim should succeed");
        drop(first_manager);

        let second_manager = QueueManager::new(queue, scope);
        let reclaimed = second_manager
            .claim_next("review:high", "consumer-b", Duration::from_secs(60))
            .unwrap()
            .expect("restart should invalidate prior-epoch lease and allow reclaim");
        assert_eq!(reclaimed.item_id, first_claim.item_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_claim_lock_scope_serializes_across_queue_managers() {
        let dir = tempdir().unwrap();
        let keyspace = Config::new(dir.path()).open().unwrap();
        let queue = keyspace
            .open_partition("queue", PartitionCreateOptions::default())
            .unwrap();

        enqueue_item(&queue, "review:high", "R001", 1).unwrap();

        let scope = dir.path().to_string_lossy().into_owned();
        let manager_a = Arc::new(QueueManager::new(queue.clone(), scope.clone()));
        let manager_b = Arc::new(QueueManager::new(queue, scope));

        let contenders = 16usize;
        let barrier = Arc::new(AsyncBarrier::new(contenders));
        let mut handles = Vec::with_capacity(contenders);

        for idx in 0..contenders {
            let barrier = Arc::clone(&barrier);
            let manager = if idx % 2 == 0 {
                Arc::clone(&manager_a)
            } else {
                Arc::clone(&manager_b)
            };

            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                manager
                    .claim_next(
                        "review:high",
                        &format!("consumer-{idx}"),
                        Duration::from_secs(60),
                    )
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
            "separate QueueManager instances sharing a store path must still produce one winner"
        );
        assert_eq!(winners[0].item_id, "R001");
    }

    #[test]
    fn test_claim_lock_scope_is_removed_after_last_manager_drops() {
        let dir = tempdir().unwrap();
        let keyspace = Config::new(dir.path()).open().unwrap();
        let queue = keyspace
            .open_partition("queue", PartitionCreateOptions::default())
            .unwrap();

        let scope = format!("claim-lock-scope-{}", Uuid::new_v4());

        {
            let manager_a = QueueManager::new(queue.clone(), scope.clone());
            let manager_b = QueueManager::new(queue, scope.clone());

            assert!(claim_lock_registry_contains_scope(&scope));

            drop(manager_a);
            assert!(claim_lock_registry_contains_scope(&scope));

            drop(manager_b);
        }

        assert!(
            !claim_lock_registry_contains_scope(&scope),
            "unused claim lock scopes should be removed after the last QueueManager drops"
        );
    }

    #[test]
    fn test_drop_race_keeps_registry_entry_for_new_same_scope_manager() {
        let dir = tempdir().unwrap();
        let keyspace = Config::new(dir.path()).open().unwrap();
        let queue = keyspace
            .open_partition("queue", PartitionCreateOptions::default())
            .unwrap();

        let scope = format!("claim-lock-scope-race-{}", Uuid::new_v4());
        let dropping_manager = QueueManager::new(queue.clone(), scope.clone());
        let drop_started = Arc::new(StdBarrier::new(2));
        let replacement_created = Arc::new(StdBarrier::new(2));
        install_claim_lock_drop_hook(
            scope.clone(),
            Arc::new({
                let drop_started = Arc::clone(&drop_started);
                let replacement_created = Arc::clone(&replacement_created);
                move || {
                    drop_started.wait();
                    replacement_created.wait();
                }
            }),
        );

        let drop_thread = std::thread::spawn(move || drop(dropping_manager));

        drop_started.wait();
        let replacement_manager = QueueManager::new(queue.clone(), scope.clone());
        replacement_created.wait();
        drop_thread.join().unwrap();

        assert!(
            claim_lock_registry_contains_scope(&scope),
            "replacement manager should keep the scope registered during a concurrent drop"
        );

        let future_manager = QueueManager::new(queue, scope);
        assert!(
            Arc::ptr_eq(&replacement_manager.claim_lock, &future_manager.claim_lock),
            "same-scope managers created after a drop race must keep sharing one lock"
        );
    }
}
