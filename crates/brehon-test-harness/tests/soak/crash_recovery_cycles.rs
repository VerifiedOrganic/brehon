//! Soak test: Repeated crash/recovery cycles.
//!
//! Simulates many crash-and-recover events to ensure that the store never
//! reuses sequence numbers, never leaves torn batches visible, and that
//! view state is always consistent with the durable event log.

use std::collections::HashSet;

use brehon_ports::EventStore;
use brehon_test_harness::InMemoryEventStore;
use brehon_types::{Event, EventFilter, EventKind, ViewOperation, ViewType, ViewUpdate};
use chrono::Utc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_crash_recovery_no_sequence_reuse() {
    let store = InMemoryEventStore::new();
    let cycles = crate::soak_cycles_locked(200);

    let mut all_ids: HashSet<u64> = HashSet::new();

    for cycle in 0..cycles {
        // Write some events
        let mut cycle_ids = Vec::new();
        for i in 0..5 {
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("cycle-{}-task-{}", cycle, i),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("cycle-{}", cycle),
            };
            let id = store.append(event).await.unwrap();
            cycle_ids.push(id.as_u64());
        }

        // Mark some as persisted (simulate partial flush)
        if cycle % 3 == 0 {
            store.mark_persisted();
        }

        // Simulate crash recovery
        let discarded = store.simulate_crash_recovery();

        // Write post-recovery events
        for i in 0..3 {
            let event = Event {
                kind: EventKind::TaskAssigned {
                    task_id: format!("post-{}-task-{}", cycle, i),
                    agent_id: "worker-1".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("post-{}", cycle),
            };
            let id = store.append(event).await.unwrap();
            cycle_ids.push(id.as_u64());
        }

        // Verify no ID reuse across all cycles
        for id in &cycle_ids {
            assert!(
                all_ids.insert(*id),
                "Cycle {}: EventId {} was reused across cycles",
                cycle,
                id
            );
        }

        // Verify post-recovery IDs (last 3) are not among the discarded IDs.
        // The first 5 IDs in cycle_ids are pre-crash; the last 3 are post-recovery.
        let post_recovery_ids: Vec<u64> = cycle_ids.iter().skip(5).copied().collect();
        for discarded_id in &discarded {
            let discarded_u64 = discarded_id.as_u64();
            assert!(
                !post_recovery_ids.contains(&discarded_u64),
                "Cycle {}: Discarded ID {} was reused immediately after recovery",
                cycle,
                discarded_u64
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_crash_recovery_views_consistent() {
    let store = InMemoryEventStore::new();
    let cycles = crate::soak_cycles_locked(200);

    for cycle in 0..cycles {
        let task_id = format!("T-{}", cycle);

        // Append baseline event and persist
        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            })
            .await
            .unwrap();
        store.mark_persisted();

        // Atomic append with view update in crash window
        let views = vec![ViewUpdate {
            view_type: ViewType::Task,
            key: task_id.clone(),
            operation: ViewOperation::Set {
                field: "status".to_string(),
                value: "InProgress".to_string(),
            },
        }];

        store
            .append_atomic(
                vec![Event {
                    kind: EventKind::TaskAssigned {
                        task_id: task_id.clone(),
                        agent_id: "worker-1".to_string(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: task_id.clone(),
                }],
                views,
            )
            .await
            .unwrap();

        // Crash before persist
        store.simulate_crash_recovery();

        // After recovery, the view should reflect only persisted state
        let view = store.get_view(&ViewType::Task, &task_id);
        assert_ne!(
            view.as_deref(),
            Some("InProgress"),
            "Cycle {}: Unpersisted view update leaked after crash",
            cycle
        );

        // The event log should contain only the persisted event
        let events = store
            .query(EventFilter::new().aggregate(&task_id))
            .await
            .unwrap();
        assert_eq!(
            events.len(),
            1,
            "Cycle {}: Only persisted event should survive",
            cycle
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_crash_recovery_atomic_batch_integrity() {
    let store = InMemoryEventStore::new();
    let cycles = crate::soak_cycles_locked(200);

    for cycle in 0..cycles {
        let task_id = format!("batch-{}", cycle);

        // Persist a baseline
        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            })
            .await
            .unwrap();
        store.mark_persisted();

        // Atomic batch of 3 events
        let batch = vec![
            Event {
                kind: EventKind::TaskAssigned {
                    task_id: task_id.clone(),
                    agent_id: "w1".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            },
            Event {
                kind: EventKind::ReviewRequested {
                    task_id: task_id.clone(),
                    review_id: format!("R-{}", cycle),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            },
            Event {
                kind: EventKind::TaskCompleted {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            },
        ];

        store.append_atomic(batch, Vec::new()).await.unwrap();

        // Crash without persist
        store.simulate_crash_recovery();

        // No partial batch should survive
        let events = store
            .query(EventFilter::new().aggregate(&task_id))
            .await
            .unwrap();
        assert_eq!(
            events.len(),
            1,
            "Cycle {}: Atomic batch should be fully discarded, not partially committed",
            cycle
        );
    }
}
