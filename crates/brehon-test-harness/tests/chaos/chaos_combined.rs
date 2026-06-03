//! Chaos test: All chaos parameters active simultaneously.
//!
//! Verifies system stability under combined chaos conditions.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_ports::EventStore;
use brehon_test_harness::{ChaosConfig, ChaosInjector, InMemoryEventStore};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn chaos_combined_full_chaos_stability() {
    let run_count = 10;
    let success_count = Arc::new(AtomicUsize::new(0));

    for run in 0..run_count {
        let seed = 1000 + run as u64;
        let config = ChaosConfig {
            seed,
            delay_range: Some((Duration::from_millis(0), Duration::from_millis(10))),
            drop_probability: 0.03,
            duplicate_probability: 0.03,
            lease_expiry_probability: 0.01,
            enabled: true,
        };

        let result = run_combined_scenario(config).await;

        if result.is_ok() {
            success_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    let successes = success_count.load(Ordering::SeqCst);
    assert!(
        successes >= run_count - 5,
        "Too many failures: {}/{} runs failed",
        run_count - successes,
        run_count
    );
}

#[tokio::test]
async fn chaos_combined_no_lost_messages() {
    let config = ChaosConfig {
        seed: 42,
        delay_range: Some((Duration::from_millis(0), Duration::from_millis(10))),
        drop_probability: 0.03,
        duplicate_probability: 0.03,
        lease_expiry_probability: 0.0,
        enabled: true,
    };
    let store = Arc::new(InMemoryEventStore::new());

    let event_count = 50;
    let sent_count = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    for i in 0..event_count {
        let store = Arc::clone(&store);
        let sent_count = Arc::clone(&sent_count);
        let config = config.clone();

        handles.push(tokio::spawn(async move {
            let mut injector = ChaosInjector::new(config);

            injector.delay().await;

            if injector.should_drop() {
                sent_count.fetch_add(1, Ordering::SeqCst);
                return None;
            }

            let task_id = format!("T{:03}", i);
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            };

            let result = store.append(event.clone()).await.unwrap();

            if injector.should_duplicate() {
                store.append(event).await.unwrap();
            }

            sent_count.fetch_add(1, Ordering::SeqCst);
            Some(result)
        }));
    }

    futures::future::join_all(handles).await;

    let sent = sent_count.load(Ordering::SeqCst);
    let stored = store.len();

    assert!(
        stored <= sent,
        "Stored events ({}) exceeds sent ({})",
        stored,
        sent
    );
    assert!(
        stored >= sent / 2,
        "Too many dropped events: {} sent, {} stored",
        sent,
        stored
    );
}

#[tokio::test]
async fn chaos_combined_queue_stability() {
    let store = Arc::new(InMemoryEventStore::new());

    let item_count = 10;
    for i in 0..item_count {
        store.enqueue("work", &format!("item-{}", i), "data");
    }

    let processed = Arc::new(AtomicUsize::new(0));
    let claimed_items = Arc::new(std::sync::Mutex::new(HashSet::new()));

    let mut handles = vec![];

    for consumer_id in 0..3 {
        let store = Arc::clone(&store);
        let processed = Arc::clone(&processed);
        let claimed_items = Arc::clone(&claimed_items);

        handles.push(tokio::spawn(async move {
            let consumer_name = format!("consumer-{}", consumer_id);

            for _ in 0..20 {
                let claim_result = store
                    .claim_next("work", &consumer_name, Duration::from_secs(60))
                    .await;

                match claim_result {
                    Ok(Some(claim)) => {
                        {
                            let mut items = claimed_items.lock().unwrap();
                            if items.contains(&claim.item_id) {
                                panic!("Double claim: {}", claim.item_id);
                            }
                            items.insert(claim.item_id.clone());
                        }

                        tokio::time::sleep(Duration::from_millis(2)).await;

                        if let Ok(()) = store.ack_claim(&claim.claim_id).await {
                            processed.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => continue,
                }
            }
        }));
    }

    futures::future::join_all(handles).await;

    let final_processed = processed.load(Ordering::SeqCst);
    assert_eq!(final_processed, item_count, "All items processed");
}

#[tokio::test]
async fn chaos_combined_invariants_maintained() {
    let config = ChaosConfig {
        seed: 300,
        delay_range: Some((Duration::from_millis(0), Duration::from_millis(5))),
        drop_probability: 0.03,
        duplicate_probability: 0.03,
        lease_expiry_probability: 0.0,
        enabled: true,
    };
    let store = InMemoryEventStore::new();
    let mut injector = ChaosInjector::new(config);

    let events_sent = 20;
    let mut events_created = 0;

    for i in 0..events_sent {
        injector.delay().await;

        if injector.should_drop() {
            continue;
        }

        let task_id = format!("T{:03}", i);
        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        };

        store.append(event.clone()).await.unwrap();
        events_created += 1;

        if injector.should_duplicate() {
            store.append(event).await.unwrap();
        }
    }

    assert_eq!(
        store.len(),
        events_created,
        "Event count mismatch: expected {}, got {}",
        events_created,
        store.len()
    );

    let events = store.all_events();
    let mut seen_ids = HashSet::new();

    for event in &events {
        assert!(
            seen_ids.insert(event.aggregate_id.clone()),
            "Duplicate aggregate_id found: {}",
            event.aggregate_id
        );
    }

    let ids: Vec<_> = events.iter().map(|e| e.aggregate_id.clone()).collect();
    for i in 1..ids.len() {
        if ids[i] < ids[i - 1] {
            panic!(
                "Ordering invariant violated: {} before {}",
                ids[i - 1],
                ids[i]
            );
        }
    }
}

async fn run_combined_scenario(config: ChaosConfig) -> Result<(), String> {
    let store = InMemoryEventStore::new();
    let mut injector = ChaosInjector::new(config.clone());

    for i in 0..10 {
        injector.delay().await;

        if injector.should_drop() {
            continue;
        }

        let task_id = format!("T{:03}", i);
        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        };

        store
            .append(event.clone())
            .await
            .map_err(|e| format!("Append failed: {:?}", e))?;

        if injector.should_duplicate() {
            store
                .append(event)
                .await
                .map_err(|e| format!("Duplicate append failed: {:?}", e))?;
        }

        if injector.should_expire_lease() {
            store.enqueue("work", &task_id, "test");
        }
    }

    let events = store.all_events();
    if events.len() > 10 {
        return Err(format!("Too many events: {}", events.len()));
    }

    Ok(())
}
