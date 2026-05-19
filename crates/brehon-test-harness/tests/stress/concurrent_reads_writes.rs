//! Stress test: 10 writers + 10 readers, verify consistent reads.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Barrier;

use brehon_ports::EventStore;
use brehon_test_harness::InMemoryEventStore;
use brehon_types::{Event, EventKind};
use chrono::Utc;

static CONSISTENT_READS: AtomicUsize = AtomicUsize::new(0);
static TOTAL_READS: AtomicUsize = AtomicUsize::new(0);
static CORRUPTED_READS: AtomicUsize = AtomicUsize::new(0);

#[tokio::test(flavor = "multi_thread", worker_threads = 24)]
async fn stress_concurrent_reads_writes_consistency() {
    CONSISTENT_READS.store(0, Ordering::SeqCst);
    TOTAL_READS.store(0, Ordering::SeqCst);
    CORRUPTED_READS.store(0, Ordering::SeqCst);

    let store = Arc::new(InMemoryEventStore::new());
    let barrier = Arc::new(Barrier::new(20));
    let max_event_id = Arc::new(AtomicU64::new(0));
    let write_complete = Arc::new(AtomicUsize::new(0));

    let writer_handles: Vec<_> = (0..10)
        .map(|writer_id| {
            let barrier = Arc::clone(&barrier);
            let store = Arc::clone(&store);
            let max_id = Arc::clone(&max_event_id);

            tokio::spawn(async move {
                barrier.wait().await;

                for i in 0..1000 {
                    let event = Event {
                        kind: EventKind::TaskCreated {
                            task_id: format!("T{}-{}", writer_id, i),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: format!("writer-{}", writer_id),
                    };

                    let event_id = store.append(event).await.unwrap();

                    let current = max_id.load(Ordering::SeqCst);
                    if event_id.as_u64() > current {
                        let _ = max_id.compare_exchange(
                            current,
                            event_id.as_u64(),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                    }
                }

                writer_id
            })
        })
        .collect();

    let reader_handles: Vec<_> = (0..10)
        .map(|reader_id| {
            let barrier = Arc::clone(&barrier);
            let store = Arc::clone(&store);
            let max_id = Arc::clone(&max_event_id);
            let complete = Arc::clone(&write_complete);

            tokio::spawn(async move {
                barrier.wait().await;

                let mut last_seen_id: u64 = 0;
                let mut consistent_count = 0;
                let mut read_count = 0;

                loop {
                    let writers_done = complete.load(Ordering::SeqCst);
                    let current_max = max_id.load(Ordering::SeqCst);

                    if writers_done >= 10 && last_seen_id >= current_max {
                        break;
                    }

                    let events = store
                        .stream(
                            if last_seen_id == 0 {
                                None
                            } else {
                                Some(brehon_types::EventId::new(last_seen_id))
                            },
                            100,
                        )
                        .await
                        .unwrap();

                    read_count += 1;
                    TOTAL_READS.fetch_add(1, Ordering::SeqCst);

                    for (_event, event_id) in &events {
                        let id = event_id.as_u64();

                        if id <= last_seen_id {
                            CORRUPTED_READS.fetch_add(1, Ordering::SeqCst);
                        } else {
                            last_seen_id = id;
                            consistent_count += 1;
                        }
                    }

                    if !events.is_empty() {
                        CONSISTENT_READS.fetch_add(events.len(), Ordering::SeqCst);
                    }

                    tokio::time::sleep(Duration::from_micros(10)).await;
                }

                (reader_id, consistent_count, read_count)
            })
        })
        .collect();

    let start = Instant::now();

    let writer_results: Vec<_> = futures::future::join_all(writer_handles).await;

    for _ in writer_results.into_iter().filter_map(|r| r.ok()) {
        write_complete.fetch_add(1, Ordering::SeqCst);
    }

    let reader_results: Vec<_> = futures::future::join_all(reader_handles).await;

    for result in reader_results.into_iter().filter_map(|r| r.ok()) {
        let reader_id = result.0;
        let _ = reader_id;
    }

    let elapsed = start.elapsed();

    let total_events = store.len();
    assert_eq!(
        total_events, 10000,
        "Expected 10000 events, got {}",
        total_events
    );

    let corrupted = CORRUPTED_READS.load(Ordering::SeqCst);
    assert_eq!(
        corrupted, 0,
        "Corrupted reads detected: {} events had non-monotonic IDs",
        corrupted
    );

    let consistent = CONSISTENT_READS.load(Ordering::SeqCst);
    let total = TOTAL_READS.load(Ordering::SeqCst);

    println!(
        "Concurrent read/write stress completed: {} events written in {:?}",
        total_events, elapsed
    );
    println!(
        "Reads: {} total calls, {} consistent events observed, {} corrupted",
        total, consistent, corrupted
    );

    assert!(elapsed.as_secs() < 30, "Test took too long: {:?}", elapsed);
}
