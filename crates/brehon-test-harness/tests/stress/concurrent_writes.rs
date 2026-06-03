//! Stress test: 20 threads, 10000 events each, verify no lost writes.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Barrier;

use brehon_ports::EventStore;
use brehon_test_harness::InMemoryEventStore;
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test(flavor = "multi_thread", worker_threads = 24)]
async fn stress_concurrent_writes_no_lost_events() {
    let store = Arc::new(InMemoryEventStore::new());
    let barrier = Arc::new(Barrier::new(20));

    let start = Instant::now();

    let handles: Vec<_> = (0..20)
        .map(|writer_id| {
            let barrier = Arc::clone(&barrier);
            let store = Arc::clone(&store);

            tokio::spawn(async move {
                barrier.wait().await;

                let mut writer_event_ids: Vec<(u64, String)> = Vec::with_capacity(10000);

                for i in 0..10000 {
                    let event = Event {
                        kind: EventKind::TaskCreated {
                            task_id: format!("T{}-{}", writer_id, i),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: format!("writer-{}", writer_id),
                    };

                    let event_id = store.append(event).await.unwrap();
                    writer_event_ids.push((event_id.as_u64(), format!("T{}-{}", writer_id, i)));
                }

                writer_event_ids
            })
        })
        .collect();

    let results: Vec<_> = futures::future::join_all(handles).await;

    let elapsed = start.elapsed();

    let total_expected = 20 * 10000;
    let actual_count = store.len();

    assert_eq!(
        actual_count, total_expected,
        "Lost writes detected: expected {} events, got {}",
        total_expected, actual_count
    );

    let mut all_event_ids: Vec<(u64, String)> = Vec::with_capacity(total_expected);
    for result in results {
        let event_ids = result.unwrap();
        all_event_ids.extend(event_ids);
    }

    all_event_ids.sort_by_key(|(id, _)| *id);

    for (i, (id, _)) in all_event_ids.iter().enumerate() {
        assert_eq!(
            *id,
            (i + 1) as u64,
            "Event ID gap at index {}: expected {}, got {}",
            i,
            i + 1,
            id
        );
    }

    for writer_id in 0..20u64 {
        let writer_events: Vec<_> = all_event_ids
            .iter()
            .filter(|(_, task)| task.starts_with(&format!("T{}-", writer_id)))
            .collect();

        assert_eq!(
            writer_events.len(),
            10000,
            "Writer {} lost events: expected 10000, got {}",
            writer_id,
            writer_events.len()
        );

        let mut last_id: u64 = 0;

        for (event_id, _) in &writer_events {
            if *event_id > last_id {
                last_id = *event_id;
            } else {
                panic!(
                    "Writer {} has out-of-order events: current {} <= last {}",
                    writer_id, event_id, last_id
                );
            }
        }
    }

    println!(
        "Stress test completed: {} events in {:?} ({:.0} events/sec)",
        total_expected,
        elapsed,
        total_expected as f64 / elapsed.as_secs_f64()
    );

    assert!(
        elapsed.as_secs() < 30,
        "Stress test took too long: {:?}",
        elapsed
    );
}
