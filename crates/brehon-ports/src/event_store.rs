//! EventStore trait for event sourcing persistence.

use async_trait::async_trait;
use std::time::Duration;

use brehon_types::{ClaimId, QueueClaim, ViewUpdate};
use brehon_types::{Event, EventFilter, EventId};

use crate::PortError;

/// Trait for event sourcing persistence.
///
/// # Global Append Ordering
///
/// Events are appended with a monotonically increasing `EventId`. This sequence
/// number is assigned by the event store at append time and is guaranteed to be:
/// - Globally unique across all events
/// - Strictly increasing (no gaps or duplicates)
/// - Durable once assigned
///
/// The ordering is consistent across all readers - if event A has a lower
/// `EventId` than event B, then A was appended before B.
///
/// # Idempotency
///
/// The `append` and `append_atomic` methods support idempotency via an optional
/// `IdempotencyKey`. If an idempotency key is provided:
/// - If the key was never seen, the append proceeds normally
/// - If the key was previously seen, the append returns the same `EventId` without
///   adding duplicate events
/// - Idempotency keys are retried indefinitely (no expiration)
///
/// # Durable Lease Semantics
///
/// Queue claims (`claim_next`) use durable leases with the following guarantees:
/// - A claim is atomic: only one consumer can claim a given item
/// - A claim has an expiration time (`lease_for` duration)
/// - Claims survive process restarts (persisted to storage)
/// - An expired claim can be re-claimed by another consumer
/// - `renew_claim` extends the lease duration for an active claim
/// - `ack_claim` marks the claim as complete and removes the item from the queue
///
/// Leases prevent double-processing under concurrent access while allowing
/// recovery from consumer failures.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Append a single event to the event log.
    ///
    /// Returns the assigned `EventId` for the newly appended event.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the append fails due to storage issues.
    async fn append(&self, event: Event) -> Result<EventId, PortError>;

    /// Append events atomically with view updates.
    ///
    /// This is a transactional operation: either all events and view updates
    /// are committed, or none are.
    ///
    /// Returns the assigned `EventId`s for all appended events.
    ///
    /// # Idempotency
    ///
    /// If the first event includes an `idempotency_key`, the entire operation
    /// is idempotent.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the atomic append fails.
    async fn append_atomic(
        &self,
        events: Vec<Event>,
        views: Vec<ViewUpdate>,
    ) -> Result<Vec<EventId>, PortError>;

    /// Atomically append an event and materialize a durable queue item.
    ///
    /// Implementations must guarantee that a successfully persisted event does
    /// not exist without its corresponding claimable queue record. When an
    /// `idempotency_key` is provided, retries must return the original
    /// `EventId` without creating a duplicate queue item.
    async fn append_and_enqueue(
        &self,
        event: Event,
        queue: &str,
        item_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<EventId, PortError>;

    /// Query events matching a filter.
    ///
    /// Returns events in append order (by `EventId`).
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the query fails.
    async fn query(&self, filter: EventFilter) -> Result<Vec<Event>, PortError>;

    /// Stream events since a point in the log.
    ///
    /// Returns up to `limit` events with `EventId` greater than `since`.
    /// If `since` is `None`, streams from the beginning.
    ///
    /// Events are returned in append order with their assigned EventIds.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the stream fails.
    async fn stream(
        &self,
        since: Option<EventId>,
        limit: usize,
    ) -> Result<Vec<(Event, EventId)>, PortError>;

    /// Claim the next item from a queue.
    ///
    /// Atomically claims an item from the named queue for the consumer.
    /// The claim is leased for the specified duration.
    ///
    /// Returns `None` if the queue is empty.
    ///
    /// # Lease Semantics
    ///
    /// - Only one consumer can claim any given item
    /// - The claim expires after `lease_for` duration
    /// - Expired claims can be re-claimed
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the claim operation fails.
    async fn claim_next(
        &self,
        queue: &str,
        consumer: &str,
        lease_for: Duration,
    ) -> Result<Option<QueueClaim>, PortError>;

    /// Acknowledge a claim as completed.
    ///
    /// Removes the claimed item from the queue permanently.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the ack fails.
    /// Returns `PortError::Storage` if the claim doesn't exist or already expired.
    async fn ack_claim(&self, claim_id: &ClaimId) -> Result<(), PortError>;

    /// Renew a claim's lease.
    ///
    /// Extends the lease duration for an active claim.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the renewal fails.
    /// Returns `PortError::Storage` if the claim doesn't exist or already expired.
    async fn renew_claim(&self, claim_id: &ClaimId, lease_for: Duration) -> Result<(), PortError>;

    /// Return the highest event id currently in the store.
    async fn high_water_mark(&self) -> Result<EventId, PortError>;

    /// Retain only events at or after `before`, removing (or archiving) older
    /// events. Returns the number of events removed.
    ///
    /// # Safety
    ///
    /// Callers must ensure all consumers have processed events up to `before`
    /// before invoking retention.
    async fn retain_events(&self, before: EventId) -> Result<usize, PortError>;

    /// Remove idempotency keys older than `older_than`.
    ///
    /// Returns the number of keys removed.
    async fn expire_idempotency_keys(&self, older_than: Duration) -> Result<usize, PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_error_variants() {
        let e1 = PortError::Storage("disk full".into());
        let e2 = PortError::Agent("timeout".into());
        let e3 = PortError::Git("conflict".into());
        let e4 = PortError::Config("invalid".into());
        let e5 = PortError::Notification("channel closed".into());
        let e6 = PortError::Runtime("channel lagged".into());
        let e7 = PortError::Policy("denied".into());
        let e8 = PortError::Detection("invalid rule".into());
        let e9 = PortError::IO("not found".into());

        assert!(matches!(e1, PortError::Storage(_)));
        assert!(matches!(e2, PortError::Agent(_)));
        assert!(matches!(e3, PortError::Git(_)));
        assert!(matches!(e4, PortError::Config(_)));
        assert!(matches!(e5, PortError::Notification(_)));
        assert!(matches!(e6, PortError::Runtime(_)));
        assert!(matches!(e7, PortError::Policy(_)));
        assert!(matches!(e8, PortError::Detection(_)));
        assert!(matches!(e9, PortError::IO(_)));
    }
}
