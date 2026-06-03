//! Priority-lane review queue.
//!
//! Queue processes higher-priority lanes first and FIFO within each lane.
//! Uses EventStore.claim_next() for durable claims with lease semantics.

use std::time::Duration;

use thiserror::Error;

use brehon_types::{ClaimId, Event, EventKind, Priority, QueueClaim};

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("Claim not found: {0}")]
    ClaimNotFound(String),

    #[error("Claim expired: {0}")]
    ClaimExpired(String),

    #[error("Queue empty")]
    QueueEmpty,

    #[error("Storage error: {0}")]
    Storage(String),
}

/// Priority lane names for the review queue.
const LANE_NAMES: [&str; 4] = ["critical", "high", "medium", "low"];

fn priority_to_lane(priority: Priority) -> &'static str {
    match priority {
        Priority::Critical => "critical",
        Priority::High => "high",
        Priority::Medium => "medium",
        Priority::Low => "low",
    }
}

fn review_requested_idempotency_key(review_id: &str) -> String {
    format!("review_requested:{review_id}")
}

#[allow(dead_code)]
fn lane_to_priority(lane: &str) -> Option<Priority> {
    match lane {
        "critical" => Some(Priority::Critical),
        "high" => Some(Priority::High),
        "medium" => Some(Priority::Medium),
        "low" => Some(Priority::Low),
        _ => None,
    }
}

/// Priority queue for review requests.
///
/// Uses durable claims via EventStore to ensure exactly-once processing
/// under concurrent access. Higher priority lanes are processed first,
/// with FIFO ordering within each lane.
pub struct PriorityQueue {
    store: std::sync::Arc<dyn brehon_ports::EventStore>,
    default_lease_duration: Duration,
}

impl PriorityQueue {
    pub fn new(store: std::sync::Arc<dyn brehon_ports::EventStore>) -> Self {
        Self {
            store,
            default_lease_duration: Duration::from_secs(300),
        }
    }

    pub fn with_lease_duration(mut self, duration: Duration) -> Self {
        self.default_lease_duration = duration;
        self
    }

    /// Enqueue a review request with the given priority.
    pub async fn enqueue(
        &self,
        review_id: &str,
        task_id: &str,
        priority: Priority,
    ) -> Result<(), QueueError> {
        let lane = priority_to_lane(priority);
        let queue_name = format!("review:{lane}");
        let idempotency_key = review_requested_idempotency_key(review_id);

        let event = Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: review_id.to_string(),
        };

        self.store
            .append_and_enqueue(event, &queue_name, review_id, Some(&idempotency_key))
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Claim the next review request from the queue.
    ///
    /// Processes lanes in priority order (critical > high > medium > low).
    /// Within each lane, processes in FIFO order.
    pub async fn claim_next(&self, consumer: &str) -> Result<Option<QueueClaim>, QueueError> {
        for lane in LANE_NAMES {
            let queue_name = format!("review:{}", lane);
            if let Some(claim) = self
                .store
                .claim_next(&queue_name, consumer, self.default_lease_duration)
                .await
                .map_err(|e| QueueError::Storage(e.to_string()))?
            {
                return Ok(Some(claim));
            }
        }

        Ok(None)
    }

    /// Claim from a specific priority lane.
    #[allow(dead_code)]
    pub async fn claim_from_lane(
        &self,
        lane: &str,
        consumer: &str,
    ) -> Result<Option<QueueClaim>, QueueError> {
        let queue_name = format!("review:{}", lane);
        self.store
            .claim_next(&queue_name, consumer, self.default_lease_duration)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))
    }

    /// Acknowledge a claim as completed.
    pub async fn ack(&self, claim_id: &ClaimId) -> Result<(), QueueError> {
        self.store
            .ack_claim(claim_id)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))
    }

    /// Renew a claim's lease (for long-running reviews).
    #[allow(dead_code)]
    pub async fn renew(&self, claim_id: &ClaimId) -> Result<(), QueueError> {
        self.store
            .renew_claim(claim_id, self.default_lease_duration)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))
    }

    /// Renew a claim with a custom duration.
    #[allow(dead_code)]
    pub async fn renew_with_duration(
        &self,
        claim_id: &ClaimId,
        duration: Duration,
    ) -> Result<(), QueueError> {
        self.store
            .renew_claim(claim_id, duration)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_priority_mapping() {
        assert_eq!(priority_to_lane(Priority::Critical), "critical");
        assert_eq!(priority_to_lane(Priority::High), "high");
        assert_eq!(priority_to_lane(Priority::Medium), "medium");
        assert_eq!(priority_to_lane(Priority::Low), "low");

        assert_eq!(lane_to_priority("critical"), Some(Priority::Critical));
        assert_eq!(lane_to_priority("high"), Some(Priority::High));
        assert_eq!(lane_to_priority("medium"), Some(Priority::Medium));
        assert_eq!(lane_to_priority("low"), Some(Priority::Low));
        assert_eq!(lane_to_priority("unknown"), None);
    }

    /// Regression: after enqueue, a review MUST be claimable from the durable
    /// queue path.
    ///
    /// This test asserts the intended behaviour: after enqueue the review
    /// should be claimable from the durable queue path.
    #[tokio::test]
    async fn enqueued_review_is_claimable_from_durable_queue() {
        let store = std::sync::Arc::new(brehon_test_harness::InMemoryEventStore::new());
        let queue = PriorityQueue::new(store.clone());

        // Enqueue a review request.
        queue
            .enqueue("R-001", "T-001", Priority::High)
            .await
            .expect("enqueue should succeed");

        // Precondition: the event was actually persisted.
        assert_eq!(
            store.len(),
            1,
            "enqueue must persist exactly one event, but store has {} events",
            store.len()
        );

        // Intended behaviour: the enqueued review must be claimable.
        let claim = queue
            .claim_next("reviewer-1")
            .await
            .expect("claim_next should not error");

        assert!(
            claim.is_some(),
            "enqueued review R-001 should be claimable from the durable queue path"
        );
    }

    #[tokio::test]
    async fn enqueue_retry_is_idempotent_and_does_not_duplicate_queue_rows() {
        let store = std::sync::Arc::new(brehon_test_harness::InMemoryEventStore::new());
        let queue = PriorityQueue::new(store.clone());

        queue
            .enqueue("R-001", "T-001", Priority::High)
            .await
            .expect("initial enqueue should succeed");
        queue
            .enqueue("R-001", "T-001", Priority::High)
            .await
            .expect("retry enqueue should succeed");

        assert_eq!(store.len(), 1, "retry must not append a duplicate event");
        assert_eq!(
            store.queue_len("review:high"),
            1,
            "retry must not create a duplicate queue row"
        );

        let claim = queue
            .claim_next("reviewer-1")
            .await
            .expect("claim_next should not error")
            .expect("review should be claimable once");
        queue
            .ack(&claim.claim_id)
            .await
            .expect("ack should succeed");

        queue
            .enqueue("R-001", "T-001", Priority::High)
            .await
            .expect("post-ack retry should still be idempotent");

        let duplicate_claim = queue
            .claim_next("reviewer-2")
            .await
            .expect("claim_next should not error");
        assert!(
            duplicate_claim.is_none(),
            "retrying an already-materialized review request must not requeue it"
        );
    }
}
