//! Soak test: Queue operations over many cycles.
//!
//! Verifies that the queue subsystem remains bounded and leak-free under
//! sustained load.  This is the soak counterpart to the chaos lease tests:
//! instead of injecting failures, we run for many iterations and assert that
//! claim counts, pending items, and store size do not grow without bound.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_ports::EventStore;
use brehon_test_harness::InMemoryEventStore;
use brehon_types::{Event, EventKind};
use chrono::Utc;

const ITEMS_PER_CYCLE: usize = 10;
const CONSUMERS_PER_CYCLE: usize = 3;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn soak_queue_claim_ack_cycles_bounded() {
    let store = Arc::new(InMemoryEventStore::new());
    let cycles = crate::soak_cycles_locked(500);

    let mut total_enqueued = 0usize;
    let mut total_claimed = 0usize;

    for cycle in 0..cycles {
        let queue_name = format!("queue-{}", cycle % 5);

        // Enqueue items
        for i in 0..ITEMS_PER_CYCLE {
            let item_id = format!("cycle-{}-item-{}", cycle, i);
            store.enqueue(&queue_name, &item_id, "payload");
            total_enqueued += 1;
        }

        // Spawn consumers that claim and ack
        let claimed_count = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        for consumer_id in 0..CONSUMERS_PER_CYCLE {
            let store = Arc::clone(&store);
            let claimed_count = Arc::clone(&claimed_count);
            let queue_name = queue_name.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    match store
                        .claim_next(
                            &queue_name,
                            &format!("consumer-{}", consumer_id),
                            Duration::from_secs(60),
                        )
                        .await
                    {
                        Ok(Some(claim)) => {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            if store.ack_claim(&claim.claim_id).await.is_ok() {
                                claimed_count.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            }));
        }

        futures::future::join_all(handles).await;
        let cycle_claimed = claimed_count.load(Ordering::SeqCst);
        total_claimed += cycle_claimed;

        // After each cycle, the queue for this lane should be empty
        assert_eq!(
            store.queue_len(&queue_name),
            0,
            "Cycle {}: queue {} should be empty after all claims acked",
            cycle,
            queue_name
        );

        // Periodic boundedness check
        if cycle % 50 == 0 {
            let pending = store.pending_count(&queue_name);
            assert_eq!(pending, 0, "Cycle {}: pending count should be zero", cycle);
        }
    }

    assert_eq!(
        total_enqueued, total_claimed,
        "Every enqueued item should be claimed and acked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn soak_queue_event_store_bounded_with_appends() {
    let store = Arc::new(InMemoryEventStore::new());
    let cycles = crate::soak_cycles_locked(500);

    for cycle in 0..cycles {
        let queue_name = format!("work-{}", cycle % 3);

        // Append events AND enqueue in the same cycle
        for i in 0..ITEMS_PER_CYCLE {
            let task_id = format!("T{}-{}", cycle, i);
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            };
            store.append(event).await.unwrap();
            store.enqueue(&queue_name, &task_id, "data");
        }

        // Drain the queue
        while let Ok(Some(claim)) = store
            .claim_next(&queue_name, "drainer", Duration::from_secs(60))
            .await
        {
            let _ = store.ack_claim(&claim.claim_id).await;
        }

        // Store must remain bounded (no duplicate events from idempotency reuse)
        let expected_events = (cycle + 1) * ITEMS_PER_CYCLE;
        assert_eq!(
            store.len(),
            expected_events,
            "Cycle {}: event store should have exactly {} events",
            cycle,
            expected_events
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn soak_queue_lease_renewal_no_leak() {
    let store = Arc::new(InMemoryEventStore::new());
    let cycles = crate::soak_cycles_locked(200);
    store.enqueue("renewal-queue", "item-1", "data");

    for cycle in 0..cycles {
        let claim = store
            .claim_next("renewal-queue", "consumer", Duration::from_millis(50))
            .await
            .unwrap()
            .expect("item should be claimable");

        // Renew the lease several times
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            store
                .renew_claim(&claim.claim_id, Duration::from_millis(50))
                .await
                .expect("renewal should succeed");
        }

        // Ack and re-enqueue for the next cycle
        store.ack_claim(&claim.claim_id).await.unwrap();
        store.enqueue("renewal-queue", &format!("item-{}", cycle + 2), "data");
    }

    // Final drain
    let final_claim = store
        .claim_next("renewal-queue", "final-consumer", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(final_claim.is_some(), "One item should remain");
}
