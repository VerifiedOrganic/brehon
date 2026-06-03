//! Crash-window tests for append_atomic tearing and EventId sequence reuse.
//!
//! These tests validate the crash-safety invariants of `append_atomic`:
//!
//! 1. **Tearing**: `append_atomic` promises all-or-nothing atomicity. If a crash
//!    occurs mid-batch, partial commits must not be visible — either all events
//!    and view updates land, or none do.
//!
//! 2. **Sequence reuse**: EventIds are monotonic sequence numbers. After a crash
//!    and recovery, no EventId that was previously assigned may be reused for a
//!    new event. This prevents two distinct events from sharing the same identity.
//!
//! The acceptance gate: these tests must fail (panic) when the underlying store
//! exposes partial commits or reuses an EventId after a simulated crash.
//!
//! # Crash simulation strategy
//!
//! InMemoryEventStore supports `mark_persisted()` / `simulate_crash_recovery()`
//! which model the crash boundary deterministically:
//!   - `mark_persisted()` records the current seq as "flushed to disk"
//!   - `simulate_crash_recovery()` rewinds the in-memory counter to the
//!     persisted value and discards unflushed events — exactly mirroring
//!     what happens when a durable store's AtomicU64 is ahead of the
//!     flushed-on-disk seq counter.

use std::collections::HashSet;

use brehon_ports::EventStore;
use brehon_test_harness::InMemoryEventStore;
use brehon_types::{Event, EventFilter, EventId, EventKind, ViewOperation, ViewType, ViewUpdate};
use chrono::Utc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_event(task_id: &str, kind: EventKind) -> Event {
    Event {
        kind,
        timestamp: Utc::now(),
        aggregate_id: task_id.to_string(),
    }
}

fn make_task_lifecycle_events(task_id: &str) -> Vec<Event> {
    vec![
        make_event(
            task_id,
            EventKind::TaskCreated {
                task_id: task_id.to_string(),
            },
        ),
        make_event(
            task_id,
            EventKind::TaskAssigned {
                task_id: task_id.to_string(),
                agent_id: "worker-1".to_string(),
            },
        ),
        make_event(
            task_id,
            EventKind::TaskCompleted {
                task_id: task_id.to_string(),
            },
        ),
    ]
}

fn make_view_update(task_id: &str) -> ViewUpdate {
    ViewUpdate {
        view_type: ViewType::Task,
        key: task_id.to_string(),
        operation: ViewOperation::Set {
            field: "status".to_string(),
            value: "Completed".to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// 1. Tearing detection: partial append_atomic commits
// ---------------------------------------------------------------------------

/// Tests that a crash mid-`append_atomic` (simulated via write-individual-events
/// + crash recovery) produces a detectable torn state.
///
/// Strategy: write events one-by-one (as `append_atomic` does internally),
/// mark only the first event as persisted, then simulate crash recovery.
/// After recovery, only the persisted event survives — the torn state is
/// deterministic.
#[tokio::test]
async fn crash_tearing_detects_partial_commit() {
    let store = InMemoryEventStore::new();
    let task_id = "T-TEAR-001";

    // Write first event and mark it as persisted (simulating flush after event 1)
    let _id1 = store
        .append(make_event(
            task_id,
            EventKind::TaskCreated {
                task_id: task_id.to_string(),
            },
        ))
        .await
        .unwrap();
    store.mark_persisted();

    // Write second event WITHOUT persisting (in the crash window)
    let id2 = store
        .append(make_event(
            task_id,
            EventKind::TaskAssigned {
                task_id: task_id.to_string(),
                agent_id: "worker-1".to_string(),
            },
        ))
        .await
        .unwrap();

    // Write third event WITHOUT persisting (also in crash window)
    let _id3 = store
        .append(make_event(
            task_id,
            EventKind::TaskCompleted {
                task_id: task_id.to_string(),
            },
        ))
        .await
        .unwrap();

    // Simulate crash: only event 1 survives
    let discarded = store.simulate_crash_recovery();

    // Verify deterministic torn state: exactly 1 event survived
    let task_events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    assert_eq!(
        task_events.len(),
        1,
        "After crash recovery, exactly 1 of 3 events should survive (the persisted one)"
    );

    // The surviving event must be the one that was persisted
    assert_eq!(
        task_events[0].kind,
        EventKind::TaskCreated {
            task_id: task_id.to_string()
        }
    );

    // Events 2 and 3 were discarded
    assert_eq!(discarded.len(), 2, "2 unflushed events should be discarded");
    assert!(discarded.contains(&id2), "id2 should be in discarded set");

    // The batch is detected as torn (partial commit visible)
    let is_torn = detect_torn_batch(&store, task_id, 3).await;
    assert!(
        is_torn,
        "Recovery should detect torn append: 1 of 3 events committed"
    );
}

/// Tests that a complete, non-crashed append_atomic produces a consistent state
/// where all events and view updates are present.
#[tokio::test]
async fn no_crash_append_atomic_is_consistent() {
    let store = InMemoryEventStore::new();
    let task_id = "T-CONSIST-001";

    let events = make_task_lifecycle_events(task_id);
    let views = vec![make_view_update(task_id)];

    let ids = store.append_atomic(events.clone(), views).await.unwrap();

    // Mark as persisted (clean shutdown)
    store.mark_persisted();

    assert_eq!(ids.len(), events.len(), "All events should get IDs");

    // All events should be present
    let task_events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();
    assert_eq!(
        task_events.len(),
        events.len(),
        "All events should be queryable after atomic append"
    );
    assert_eq!(
        store.get_view(&ViewType::Task, task_id).as_deref(),
        Some("Completed"),
        "Atomic append should durably apply the task view update"
    );

    // Event IDs should be monotonically increasing
    for pair in ids.windows(2) {
        assert!(
            pair[0] < pair[1],
            "Event IDs must be monotonically increasing"
        );
    }

    // No tearing should be detected
    let is_torn = detect_torn_batch(&store, task_id, events.len()).await;
    assert!(
        !is_torn,
        "Complete atomic append should not be detected as torn"
    );
}

/// Tests that append_atomic with an empty event list returns successfully
/// with no events or view mutations.
#[tokio::test]
async fn append_atomic_empty_batch_is_noop() {
    let store = InMemoryEventStore::new();

    let ids = store.append_atomic(Vec::new(), Vec::new()).await.unwrap();
    assert!(ids.is_empty(), "Empty batch should return no IDs");
    assert!(
        store.is_empty(),
        "Store should remain empty after empty atomic append"
    );
}

/// Tests that after a torn append (crash before persist), the store reveals
/// only the persisted subset, exposing the inconsistency of a partially
/// committed atomic batch.
#[tokio::test]
async fn crash_tearing_view_not_updated_after_partial_commit() {
    let store = InMemoryEventStore::new();
    let task_id = "T-TEAR-VIEW-001";

    // Persist a baseline event, then apply an unpersisted append_atomic batch
    // whose view mutation must be rolled back on recovery.
    store
        .append(make_event(
            task_id,
            EventKind::TaskCreated {
                task_id: task_id.to_string(),
            },
        ))
        .await
        .unwrap();
    store.mark_persisted();

    store
        .append_atomic(
            vec![
                make_event(
                    task_id,
                    EventKind::TaskAssigned {
                        task_id: task_id.to_string(),
                        agent_id: "worker-1".to_string(),
                    },
                ),
                make_event(
                    task_id,
                    EventKind::TaskCompleted {
                        task_id: task_id.to_string(),
                    },
                ),
            ],
            vec![make_view_update(task_id)],
        )
        .await
        .unwrap();
    assert_eq!(
        store.get_view(&ViewType::Task, task_id).as_deref(),
        Some("Completed"),
        "The crash-window batch should have applied its view update before recovery"
    );

    // Simulate crash recovery: only the persisted event survives
    store.simulate_crash_recovery();

    let task_events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();
    assert_eq!(
        task_events.len(),
        1,
        "Only 1 event (the persisted one) should survive"
    );
    assert_eq!(
        store.get_view(&ViewType::Task, task_id),
        None,
        "Crash recovery must restore the durable view snapshot and discard leaked view updates"
    );

    // The batch is torn: 1 of 3 expected events present
    let is_torn = detect_torn_batch(&store, task_id, 3).await;
    assert!(
        is_torn,
        "Partial events (1 of 3) should be detected as torn batch"
    );
}

/// Tests that a torn append_atomic where 0 events survive (crash before any
/// persist) is also detected as a valid recovery state (no partial commits).
#[tokio::test]
async fn crash_tearing_no_persist_means_empty_recovery() {
    let store = InMemoryEventStore::new();
    let task_id = "T-TEAR-EMPTY-001";

    // Write all 3 events WITHOUT persisting (entire batch in crash window)
    for event in make_task_lifecycle_events(task_id) {
        store.append(event).await.unwrap();
    }

    // No mark_persisted() — simulate crash before any flush
    let discarded = store.simulate_crash_recovery();
    assert_eq!(
        discarded.len(),
        3,
        "All 3 unflushed events should be discarded"
    );

    let task_events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();
    assert!(
        task_events.is_empty(),
        "No events should survive if nothing was persisted"
    );

    // This is NOT torn — it's a clean "nothing committed" state
    let is_torn = detect_torn_batch(&store, task_id, 3).await;
    assert!(
        !is_torn,
        "No events surviving should NOT be detected as torn (clean empty state)"
    );
}

// ---------------------------------------------------------------------------
// 2. Sequence reuse: EventId must never be reused after crash+recovery
// ---------------------------------------------------------------------------

/// Tests that after a crash recovery that rewinds the sequence counter,
/// new EventIds must not collide with ANY previously assigned ID —
/// including IDs from events that were discarded during recovery.
///
/// This is the core sequence-reuse regression test. The bug scenario:
///   1. Events are written, in-memory counter advances to N
///   2. Crash occurs before persist, persisted_seq stays at M < N
///   3. On recovery, counter rewinds to M+1
///   4. New events get IDs M+1, M+2, ... which collide with
///      the discarded events that had those same IDs
#[tokio::test]
async fn crash_no_eventid_reuse_after_recovery() {
    let store = InMemoryEventStore::new();

    // Phase 1: Write 2 events, persist them
    let persisted_id1 = store
        .append(make_event(
            "T-REUSE-001",
            EventKind::TaskCreated {
                task_id: "T-REUSE-001".to_string(),
            },
        ))
        .await
        .unwrap();
    let persisted_id2 = store
        .append(make_event(
            "T-REUSE-002",
            EventKind::TaskCreated {
                task_id: "T-REUSE-002".to_string(),
            },
        ))
        .await
        .unwrap();
    store.mark_persisted();

    // Phase 2: Write 2 more events WITHOUT persisting (in crash window)
    let unflushed_id3 = store
        .append(make_event(
            "T-REUSE-003",
            EventKind::TaskCreated {
                task_id: "T-REUSE-003".to_string(),
            },
        ))
        .await
        .unwrap();
    let unflushed_id4 = store
        .append(make_event(
            "T-REUSE-004",
            EventKind::TaskCreated {
                task_id: "T-REUSE-004".to_string(),
            },
        ))
        .await
        .unwrap();

    // Track ALL pre-crash assigned IDs (both persisted and unflushed)
    let all_pre_crash_ids: HashSet<EventId> =
        [persisted_id1, persisted_id2, unflushed_id3, unflushed_id4]
            .into_iter()
            .collect();

    // Phase 3: Simulate crash recovery
    let discarded = store.simulate_crash_recovery();
    assert_eq!(discarded.len(), 2, "2 unflushed events should be discarded");
    assert!(discarded.contains(&unflushed_id3));
    assert!(discarded.contains(&unflushed_id4));

    // Phase 4: Write new events after recovery
    let post_recovery_id5 = store
        .append(make_event(
            "T-REUSE-005",
            EventKind::TaskCreated {
                task_id: "T-REUSE-005".to_string(),
            },
        ))
        .await
        .unwrap();
    let post_recovery_id6 = store
        .append(make_event(
            "T-REUSE-006",
            EventKind::TaskCreated {
                task_id: "T-REUSE-006".to_string(),
            },
        ))
        .await
        .unwrap();

    // REGRESSION GATE: post-recovery IDs must not collide with ANY
    // previously assigned ID, including the unflushed ones.
    for new_id in &[post_recovery_id5, post_recovery_id6] {
        assert!(
            !all_pre_crash_ids.contains(new_id),
            "Post-recovery EventId {} must not collide with any pre-crash ID {:?} — \
             sequence-reuse bug detected",
            new_id,
            all_pre_crash_ids,
        );
    }

    // Post-recovery IDs must also be strictly greater than the persisted max
    assert!(
        post_recovery_id5 > persisted_id2,
        "Post-recovery EventId ({}) must be strictly greater than last persisted ID ({})",
        post_recovery_id5,
        persisted_id2,
    );
}

/// Tests that the sequence counter after crash recovery assigns IDs that
/// are unique relative to ALL events that remain in the store after recovery.
#[tokio::test]
async fn crash_seq_counter_must_not_reuse_ids() {
    let store = InMemoryEventStore::new();

    // Write first event, persist it
    let id_first = store
        .append(make_event(
            "T-SEQREUSE-001",
            EventKind::TaskCreated {
                task_id: "T-SEQREUSE-001".to_string(),
            },
        ))
        .await
        .unwrap();
    store.mark_persisted();

    // Write second event in crash window (not persisted)
    let id_unflushed = store
        .append(make_event(
            "T-SEQREUSE-002",
            EventKind::TaskAssigned {
                task_id: "T-SEQREUSE-001".to_string(),
                agent_id: "worker-1".to_string(),
            },
        ))
        .await
        .unwrap();

    // Crash + recovery: rewinds to persisted state
    let _discarded = store.simulate_crash_recovery();

    // Write a new event after recovery
    let id_after_recovery = store
        .append(make_event(
            "T-SEQREUSE-003",
            EventKind::TaskCreated {
                task_id: "T-SEQREUSE-003".to_string(),
            },
        ))
        .await
        .unwrap();

    // The new EventId must NOT equal either the persisted or unflushed IDs
    assert_ne!(
        id_after_recovery, id_first,
        "Post-recovery EventId ({}) must not reuse the persisted ID ({})",
        id_after_recovery, id_first,
    );
    assert_ne!(
        id_after_recovery, id_unflushed,
        "Post-recovery EventId ({}) must not reuse the unflushed ID ({}) — \
         this is the sequence-reuse regression",
        id_after_recovery, id_unflushed,
    );

    // The new ID must be strictly greater than all pre-crash IDs
    assert!(
        id_after_recovery > id_first,
        "Post-recovery EventId ({}) must be strictly greater than persisted ID ({})",
        id_after_recovery,
        id_first,
    );
}

/// Tests that EventIds across multiple append_atomic calls are strictly
/// monotonically increasing with no gaps or reuse, even under simulated crash.
#[tokio::test]
async fn crash_monotonic_event_ids_across_batches() {
    let store = InMemoryEventStore::new();

    let mut all_ids: Vec<EventId> = Vec::new();
    let mut expected_seq = 1u64;

    // Write several atomic batches
    for batch_idx in 0..5 {
        let task_id = format!("T-MONO-{}", batch_idx);
        let events = vec![
            make_event(
                &task_id,
                EventKind::TaskCreated {
                    task_id: task_id.clone(),
                },
            ),
            make_event(
                &task_id,
                EventKind::TaskAssigned {
                    task_id: task_id.clone(),
                    agent_id: format!("worker-{}", batch_idx),
                },
            ),
        ];

        let ids = store.append_atomic(events, Vec::new()).await.unwrap();
        all_ids.extend(ids.iter().copied());
    }

    // Verify strict monotonicity
    for (i, id) in all_ids.iter().enumerate() {
        assert_eq!(
            id.as_u64(),
            expected_seq,
            "EventId at position {} should be {} but got {} — sequence gap or reuse detected",
            i,
            expected_seq,
            id.as_u64(),
        );
        expected_seq += 1;
    }
}

/// Tests that a torn append_atomic (where some events are lost to crash)
/// does NOT produce duplicate EventIds when the batch is retried after recovery.
///
/// This models the scenario: crash mid-batch, recover, retry the same batch.
/// The retry must not reuse any ID from the original attempt.
#[tokio::test]
async fn crash_torn_batch_retry_no_duplicate_ids() {
    let store = InMemoryEventStore::new();
    let task_id = "T-RETRY-001";

    // Write first event of a 3-event batch, persist it
    let partial_id = store
        .append(make_event(
            task_id,
            EventKind::TaskCreated {
                task_id: task_id.to_string(),
            },
        ))
        .await
        .unwrap();
    store.mark_persisted();

    // Write remaining events in crash window (not persisted)
    let unflushed_id2 = store
        .append(make_event(
            task_id,
            EventKind::TaskAssigned {
                task_id: task_id.to_string(),
                agent_id: "worker-1".to_string(),
            },
        ))
        .await
        .unwrap();
    let unflushed_id3 = store
        .append(make_event(
            task_id,
            EventKind::TaskCompleted {
                task_id: task_id.to_string(),
            },
        ))
        .await
        .unwrap();

    // Track all IDs from the original attempt.
    let original_ids: HashSet<EventId> = [partial_id, unflushed_id2, unflushed_id3]
        .into_iter()
        .collect();

    // Simulate crash recovery
    store.simulate_crash_recovery();

    // Retry the full atomic batch after recovery
    let full_events = make_task_lifecycle_events(task_id);
    let retry_ids = store.append_atomic(full_events, Vec::new()).await.unwrap();

    // The retry batch IDs must NOT overlap with ANY original IDs.
    for retry_id in &retry_ids {
        assert!(
            !original_ids.contains(retry_id),
            "Retry after torn append must not reuse EventId {} from the original attempt (original IDs: {:?})",
            retry_id,
            original_ids,
        );
    }

    // All IDs in the retry batch must be unique among themselves
    let retry_set: HashSet<EventId> = retry_ids.iter().copied().collect();
    assert_eq!(
        retry_ids.len(),
        retry_set.len(),
        "Retry batch IDs must all be unique"
    );
}

/// Tests that after crash recovery, the store's persisted_seq matches
/// the actual events that survived, and the next_id counter is correctly
/// positioned to avoid ID collisions.
#[tokio::test]
async fn crash_recovery_state_is_consistent() {
    let store = InMemoryEventStore::new();

    // Write 3 events, persist after 2
    store
        .append(make_event(
            "T-CR-1",
            EventKind::TaskCreated {
                task_id: "T-CR-1".to_string(),
            },
        ))
        .await
        .unwrap();
    store
        .append(make_event(
            "T-CR-2",
            EventKind::TaskCreated {
                task_id: "T-CR-2".to_string(),
            },
        ))
        .await
        .unwrap();
    store.mark_persisted();

    store
        .append(make_event(
            "T-CR-3",
            EventKind::TaskCreated {
                task_id: "T-CR-3".to_string(),
            },
        ))
        .await
        .unwrap();

    // Before crash: next_id=4, persisted_seq=2, high_water_mark=3
    assert_eq!(store.current_seq(), 4, "Next ID should be 4 before crash");
    assert_eq!(
        store.persisted_seq(),
        2,
        "Persisted seq should be 2 before crash"
    );

    // Crash recovery
    let discarded = store.simulate_crash_recovery();
    assert_eq!(discarded.len(), 1, "1 unflushed event should be discarded");

    // After recovery: next_id = high_water_mark + 1 = 4 (avoids reusing ID 3)
    assert_eq!(
        store.current_seq(),
        4,
        "Next ID should be 4 after recovery (high_water_mark + 1, avoiding reuse)"
    );
    assert_eq!(
        store.persisted_seq(),
        3,
        "Persisted seq should be 3 after recovery (high_water_mark)"
    );

    // Only 2 events survived
    assert_eq!(
        store.len(),
        2,
        "Only 2 events should survive after recovery"
    );

    // Next event gets ID 4 — must not collide with the discarded event (ID 3)
    let new_id = store
        .append(make_event(
            "T-CR-4",
            EventKind::TaskCreated {
                task_id: "T-CR-4".to_string(),
            },
        ))
        .await
        .unwrap();

    // The critical check: new_id != discarded[0]. With the high_water_mark
    // recovery, new_id = 4 and discarded[0] = 3 — no reuse.
    // If someone removes the high_water_mark logic and naively rewinds to
    // persisted_seq + 1 = 3, new_id would be 3 == discarded[0], and this
    // assert would fire — catching the sequence-reuse regression.
    assert_ne!(
        new_id, discarded[0],
        "Post-recovery EventId ({}) must not equal the discarded event's ID ({}) — \
         this is the sequence-reuse regression gate",
        new_id, discarded[0],
    );
}

// ---------------------------------------------------------------------------
// 3. Recovery detection helpers
// ---------------------------------------------------------------------------

/// Detect whether a batch of N events was torn (partially committed).
///
/// A batch is considered torn if some (but not all) of the expected events
/// are present — i.e., a partial commit occurred.
///
/// Returns `true` if tearing is detected.
async fn detect_torn_batch(
    store: &InMemoryEventStore,
    task_id: &str,
    expected_event_count: usize,
) -> bool {
    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    let event_count = events.len();

    // Torn if partially committed: 0 < event_count < expected
    event_count > 0 && event_count < expected_event_count
}
