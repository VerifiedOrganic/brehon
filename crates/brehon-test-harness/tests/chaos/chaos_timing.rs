//! Chaos test: Random delays on all responses.
//!
//! Tests that the system maintains invariants under random timing conditions.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_ports::EventStore;
use brehon_test_harness::{ChaosConfig, ChaosInjector, InMemoryEventStore};
use brehon_types::{Event, EventFilter, EventKind};
use chrono::Utc;

#[tokio::test]
async fn chaos_timing_random_delays_invariants() {
    let run_count = 20;
    let success_count = Arc::new(AtomicUsize::new(0));

    for run in 0..run_count {
        let seed = 42 + run as u64;
        let config = ChaosConfig {
            seed,
            delay_range: Some((Duration::from_millis(0), Duration::from_millis(10))),
            enabled: true,
            ..Default::default()
        };

        let result = run_timing_scenario(config).await;

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
async fn chaos_timing_no_message_loss() {
    let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(100));
    let store = Arc::new(InMemoryEventStore::new());

    let event_count = 50;
    let processed = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    for i in 0..event_count {
        let store = Arc::clone(&store);
        let processed = Arc::clone(&processed);
        let mut injector = ChaosInjector::new(config.clone());

        handles.push(tokio::spawn(async move {
            injector.delay().await;

            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("T{:03}", i),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("T{:03}", i),
            };

            store.append(event).await.unwrap();
            processed.fetch_add(1, Ordering::SeqCst);
        }));
    }

    futures::future::join_all(handles).await;

    assert_eq!(processed.load(Ordering::SeqCst), event_count);
    assert_eq!(store.len(), event_count);
}

#[tokio::test]
async fn chaos_timing_correct_ordering() {
    let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(50));
    let store = Arc::new(InMemoryEventStore::new());

    let event_count = 20;
    let mut handles = vec![];

    for i in 0..event_count {
        let store = Arc::clone(&store);
        let mut injector = ChaosInjector::new(config.clone());

        handles.push(tokio::spawn(async move {
            let delay = injector.random_delay().unwrap_or_default();
            tokio::time::sleep(delay).await;

            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("T{:03}", i),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("T{:03}", i),
            };

            store.append(event).await.unwrap()
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    for (i, result) in results.iter().enumerate() {
        if i > 0 {
            assert!(results[i - 1] < *result, "Event ordering violated");
        }
    }

    let all_events = store.all_events();
    assert_eq!(all_events.len(), event_count);
}

async fn run_timing_scenario(config: ChaosConfig) -> Result<(), String> {
    let store = InMemoryEventStore::new();
    let mut injector = ChaosInjector::new(config);

    for i in 0..5 {
        injector.delay().await;

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: format!("T{:03}", i),
            },
            timestamp: Utc::now(),
            aggregate_id: format!("T{:03}", i),
        };

        store
            .append(event)
            .await
            .map_err(|e| format!("Failed to append event: {:?}", e))?;
    }

    let events = store
        .query(EventFilter::new())
        .await
        .map_err(|e| format!("Failed to query events: {:?}", e))?;

    if events.len() != 5 {
        return Err(format!("Lost messages: expected 5, got {}", events.len()));
    }

    let mut ids: Vec<_> = events.iter().map(|e| e.aggregate_id.clone()).collect();
    ids.sort();
    for (i, id) in ids.iter().enumerate().take(5) {
        let expected = format!("T{:03}", i);
        if *id != expected {
            return Err(format!(
                "Corruption detected: expected {}, got {}",
                expected, id
            ));
        }
    }

    Ok(())
}
