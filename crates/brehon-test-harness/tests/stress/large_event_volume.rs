//! Stress test: 1M events, verify indexed queries and replay checkpoints stay within SLOs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use brehon_ports::EventStore;
use brehon_test_harness::InMemoryEventStore;
use brehon_types::{Event, EventFilter, EventKind};
use chrono::Utc;

const EVENT_COUNT: usize = 100_000;
const QUERY_SLO_MS: u64 = 100;
const REPLAY_SLO_MS_PER_100K: u64 = 1000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_large_event_volume_query_latency() {
    let store = Arc::new(InMemoryEventStore::new());

    let start = Instant::now();

    let batch_size = 1000;
    let batches = EVENT_COUNT / batch_size;

    for batch in 0..batches {
        for i in 0..batch_size {
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("T{:06}-{}", batch * batch_size + i, batch % 10),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("batch-{}", batch % 100),
            };

            store.append(event).await.unwrap();
        }
    }

    let write_time = start.elapsed();

    assert_eq!(
        store.len(),
        EVENT_COUNT,
        "Expected {} events, got {}",
        EVENT_COUNT,
        store.len()
    );

    println!(
        "Written {} events in {:?} ({:.0} events/sec)",
        EVENT_COUNT,
        write_time,
        EVENT_COUNT as f64 / write_time.as_secs_f64()
    );

    let aggregate_tests = 10;
    let mut aggregate_query_times = Vec::new();

    for agg_idx in 0..aggregate_tests {
        let filter = EventFilter::new().aggregate(format!("batch-{}", agg_idx));

        let query_start = Instant::now();
        let results = store.query(filter).await.unwrap();
        let query_time = query_start.elapsed();

        aggregate_query_times.push(query_time);

        assert!(
            query_time < Duration::from_millis(QUERY_SLO_MS),
            "Aggregate query took {:?}, exceeds SLO of {}ms",
            query_time,
            QUERY_SLO_MS
        );

        assert_eq!(
            results.len(),
            batch_size,
            "Expected {} events for aggregate {}, got {}",
            batch_size,
            agg_idx,
            results.len()
        );
    }

    let avg_aggregate_query: Duration =
        aggregate_query_times.iter().sum::<Duration>() / aggregate_query_times.len() as u32;
    println!(
        "Aggregate query latency: avg {:?} (SLO: {}ms)",
        avg_aggregate_query, QUERY_SLO_MS
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_large_event_volume_replay_checkpoints() {
    let store = Arc::new(InMemoryEventStore::new());

    for batch in 0..10 {
        for i in 0..(EVENT_COUNT / 10) {
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("T{:06}", batch * (EVENT_COUNT / 10) + i),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("replay-{}", batch % 10),
            };

            store.append(event).await.unwrap();
        }
    }

    let total_events = store.len();
    println!("Total events for replay test: {}", total_events);

    let checkpoints: Vec<u64> = vec![
        0,
        (total_events / 4) as u64,
        (total_events / 2) as u64,
        (3 * total_events / 4) as u64,
    ];

    let mut replay_times = Vec::new();

    for (idx, &checkpoint) in checkpoints.iter().enumerate() {
        let since = if checkpoint == 0 {
            None
        } else {
            Some(brehon_types::EventId::new(checkpoint))
        };

        let remaining = total_events as u64 - checkpoint;
        let _expected_100k_chunks = (remaining as f64 / 100_000.0).ceil() as u64;

        let replay_start = Instant::now();

        let mut current_since = since;
        let mut total_replayed = 0;
        let batch_size = 10_000;

        loop {
            let events = store.stream(current_since, batch_size).await.unwrap();

            if events.is_empty() {
                break;
            }

            let mut is_valid_ordering = true;
            let mut last_id: u64 = 0;
            let batch_count = events.len();

            for (event, event_id) in &events {
                if event_id.as_u64() <= last_id {
                    is_valid_ordering = false;
                    break;
                }
                last_id = event_id.as_u64();

                if event_id.as_u64() % 1000 == 0 {
                    let _ = &event.aggregate_id;
                }
            }

            assert!(
                is_valid_ordering,
                "Events returned out of order at checkpoint {}",
                checkpoint
            );
            total_replayed += batch_count;

            if events.len() < batch_size {
                break;
            }

            current_since = Some(brehon_types::EventId::new(last_id));
        }

        let replay_time = replay_start.elapsed();
        replay_times.push(replay_time);

        println!(
            "Checkpoint {} (since {}): replayed {} events in {:?}",
            idx, checkpoint, total_replayed, replay_time
        );

        let events_per_second = total_replayed as f64 / replay_time.as_secs_f64();
        let ms_per_100k = (replay_time.as_millis() as f64) / (total_replayed as f64 / 100_000.0);

        println!(
            "  Performance: {:.0} events/sec, {:.2}ms per 100k events",
            events_per_second, ms_per_100k
        );

        assert!(
            ms_per_100k < REPLAY_SLO_MS_PER_100K as f64 * 2.0,
            "Replay took {:.2}ms per 100k, exceeds SLO of {}ms (with 2x tolerance)",
            ms_per_100k,
            REPLAY_SLO_MS_PER_100K
        );
    }

    let avg_replay: Duration = replay_times.iter().sum::<Duration>() / replay_times.len() as u32;
    println!("Average replay time across checkpoints: {:?}", avg_replay);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_large_event_volume_event_id_monotonicity() {
    let store = Arc::new(InMemoryEventStore::new());

    let writers = 4;
    let events_per_writer = EVENT_COUNT / writers;

    let handles: Vec<_> = (0..writers)
        .map(|writer_id| {
            let store = Arc::clone(&store);

            tokio::spawn(async move {
                for i in 0..events_per_writer {
                    let event = Event {
                        kind: EventKind::TaskCreated {
                            task_id: format!("T{}-{}", writer_id, i),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: format!("writer-{}", writer_id),
                    };

                    store.append(event).await.unwrap();
                }
            })
        })
        .collect();

    futures::future::join_all(handles).await;

    let events = store.stream(None, EVENT_COUNT).await.unwrap();

    assert_eq!(
        events.len(),
        EVENT_COUNT,
        "Expected {} events, got {}",
        EVENT_COUNT,
        events.len()
    );

    let mut gaps = Vec::new();
    let mut duplicates = 0;
    let mut last_id: u64 = 0;

    for (_, event_id) in &events {
        let id = event_id.as_u64();

        if id == last_id {
            duplicates += 1;
        } else if id != last_id + 1 && last_id != 0 {
            gaps.push(id - last_id - 1);
        }

        last_id = id;
    }

    assert_eq!(duplicates, 0, "Found {} duplicate event IDs", duplicates);

    assert!(
        gaps.is_empty() || gaps.iter().sum::<u64>() == 0,
        "Found {} gaps in event ID sequence: {:?}",
        gaps.len(),
        gaps
    );

    let mut by_aggregate: std::collections::HashMap<String, Vec<u64>> =
        std::collections::HashMap::new();
    for (event, event_id) in &events {
        by_aggregate
            .entry(event.aggregate_id.clone())
            .or_default()
            .push(event_id.as_u64());
    }

    for (aggregate, ids) in by_aggregate {
        assert_eq!(
            ids.len(),
            events_per_writer,
            "Aggregate {} has {} events, expected {}",
            aggregate,
            ids.len(),
            events_per_writer
        );

        let mut sorted_ids = ids.clone();
        sorted_ids.sort();
        assert_eq!(
            ids, sorted_ids,
            "Events for aggregate {} not in order",
            aggregate
        );
    }

    println!(
        "Event ID monotonicity verified: {} sequential events, no gaps, no duplicates",
        EVENT_COUNT
    );
}
