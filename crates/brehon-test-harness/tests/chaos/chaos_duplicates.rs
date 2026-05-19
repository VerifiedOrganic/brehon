//! Chaos test: 3% duplicate message probability.
//!
//! Verifies idempotent handling of duplicate messages.

use std::collections::HashSet;
use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{ChaosConfig, ChaosInjector, InMemoryEventStore};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn chaos_duplicates_idempotent_handling() {
    let config = ChaosConfig::with_duplicates(0.03);
    let store = Arc::new(InMemoryEventStore::new());

    let task_count = 100;
    let mut injector = ChaosInjector::new(config.clone());

    for i in 0..task_count {
        let task_id = format!("T{:03}", i);

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        };

        store.append(event.clone()).await.unwrap();

        if injector.should_duplicate() {
            store.append(event).await.unwrap();
        }
    }

    assert_eq!(
        store.len(),
        task_count,
        "Idempotency should prevent duplicates"
    );
}

#[tokio::test]
async fn chaos_duplicates_concurrent_handling() {
    let config = ChaosConfig::with_duplicates(0.05);
    let store = Arc::new(InMemoryEventStore::new());

    let task_count = 50;
    let mut handles = vec![];

    for i in 0..task_count {
        let store = Arc::clone(&store);
        let mut injector = ChaosInjector::new(config.clone());

        handles.push(tokio::spawn(async move {
            let task_id = format!("T{:03}", i);

            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            };

            store.append(event.clone()).await.unwrap();

            if injector.should_duplicate() {
                store.append(event).await.unwrap();
            }
        }));
    }

    futures::future::join_all(handles).await;

    assert_eq!(
        store.len(),
        task_count,
        "Idempotency keys should prevent concurrent duplicates"
    );
}

#[tokio::test]
async fn chaos_duplicates_each_event_once() {
    let config = ChaosConfig::with_duplicates(0.10);
    let store = InMemoryEventStore::new();
    let mut injector = ChaosInjector::new(config);

    let event_count = 30;

    for i in 0..event_count {
        let task_id = format!("T{:03}", i);

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        };

        let id1 = store.append(event.clone()).await.unwrap();

        if injector.should_duplicate() {
            let id2 = store.append(event).await.unwrap();
            assert_eq!(id1, id2, "Duplicate should return same event ID");
        }
    }

    assert_eq!(
        store.len(),
        event_count,
        "Store should contain exactly {} events, got {}",
        event_count,
        store.len()
    );

    let events = store.all_events();
    let task_ids: HashSet<_> = events.iter().map(|e| e.aggregate_id.clone()).collect();

    assert_eq!(
        task_ids.len(),
        event_count,
        "Each event processed exactly once"
    );
}

#[tokio::test]
async fn chaos_duplicates_with_reviews() {
    let config = ChaosConfig::with_duplicates(0.05);
    let store = InMemoryEventStore::new();
    let mut injector = ChaosInjector::new(config);

    for i in 0..20 {
        let task_id = format!("T{:03}", i);
        let review_id = format!("R{:03}", i);

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        };
        store.append(event).await.unwrap();

        let review_event = Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.clone(),
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: format!("{}:review", task_id),
        };

        store.append(review_event.clone()).await.unwrap();

        if injector.should_duplicate() {
            store.append(review_event).await.unwrap();
        }
    }

    assert_eq!(
        store.len(),
        40,
        "Should have 40 events (20 tasks + 20 reviews)"
    );

    let events = store.all_events();
    let review_count = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReviewRequested { .. }))
        .count();

    assert_eq!(review_count, 20, "Each review should be processed once");
}
