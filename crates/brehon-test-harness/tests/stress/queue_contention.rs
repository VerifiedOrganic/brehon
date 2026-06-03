//! Stress test: 10 consumers claiming from same review queue, verify no double-claims.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Barrier;

use brehon_ports::EventStore;
use brehon_test_harness::InMemoryEventStore;

static DUPLICATE_CLAIMS: AtomicUsize = AtomicUsize::new(0);
static SUCCESSFUL_CLAIMS: AtomicUsize = AtomicUsize::new(0);

#[tokio::test(flavor = "multi_thread", worker_threads = 24)]
async fn stress_queue_contention_no_double_claims() {
    DUPLICATE_CLAIMS.store(0, Ordering::SeqCst);
    SUCCESSFUL_CLAIMS.store(0, Ordering::SeqCst);

    let store = Arc::new(InMemoryEventStore::new());

    let item_count = 100;
    for i in 0..item_count {
        store.enqueue("review:high", &format!("R{:03}", i), "review_data");
    }

    assert_eq!(store.queue_len("review:high"), item_count as usize);

    let barrier = Arc::new(Barrier::new(10));
    let processed_items = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let claim_times = Arc::new(std::sync::Mutex::new(Vec::new()));

    let start = Instant::now();

    let handles: Vec<_> = (0..10)
        .map(|consumer_id| {
            let barrier = Arc::clone(&barrier);
            let store = Arc::clone(&store);
            let processed = Arc::clone(&processed_items);
            let times = Arc::clone(&claim_times);

            tokio::spawn(async move {
                barrier.wait().await;

                let mut claims_made = 0;
                let mut total_claim_time = Duration::ZERO;

                loop {
                    let claim_start = Instant::now();

                    let claim = store
                        .claim_next(
                            "review:high",
                            &format!("consumer-{}", consumer_id),
                            Duration::from_secs(60),
                        )
                        .await
                        .unwrap();

                    let claim_elapsed = claim_start.elapsed();

                    if let Some(claim) = claim {
                        {
                            let mut processed = processed.lock().unwrap();
                            if processed.contains(&claim.item_id) {
                                DUPLICATE_CLAIMS.fetch_add(1, Ordering::SeqCst);
                                panic!("Double claim detected for item {}", claim.item_id);
                            }
                            processed.insert(claim.item_id.clone());
                        }

                        claims_made += 1;
                        total_claim_time += claim_elapsed;
                        times.lock().unwrap().push(claim_elapsed);

                        tokio::time::sleep(Duration::from_millis(1)).await;

                        let _ = store.ack_claim(&claim.claim_id).await;

                        SUCCESSFUL_CLAIMS.fetch_add(1, Ordering::SeqCst);
                    } else {
                        break;
                    }
                }

                (consumer_id, claims_made, total_claim_time)
            })
        })
        .collect();

    let results: Vec<_> = futures::future::join_all(handles).await;

    let elapsed = start.elapsed();

    let total_claims: usize = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|(_, claims, _)| claims)
        .sum::<usize>();

    assert_eq!(
        total_claims, item_count as usize,
        "Total claims {} does not match item count {}",
        total_claims, item_count
    );

    let duplicates = DUPLICATE_CLAIMS.load(Ordering::SeqCst);
    assert_eq!(
        duplicates, 0,
        "Double claims detected: {} items were claimed more than once",
        duplicates
    );

    assert_eq!(
        store.queue_len("review:high"),
        0,
        "Queue should be empty after all claims processed"
    );

    let successful = SUCCESSFUL_CLAIMS.load(Ordering::SeqCst);
    assert_eq!(
        successful, item_count as usize,
        "Successful claims {} does not match item count {}",
        successful, item_count
    );

    let times = claim_times.lock().unwrap();
    let avg_claim_time: Duration = times.iter().sum::<Duration>() / times.len() as u32;
    let max_claim_time = times.iter().max().copied().unwrap_or(Duration::ZERO);

    println!(
        "Queue contention stress completed: {} items processed by 10 consumers in {:?}",
        item_count, elapsed
    );
    println!(
        "Claim times: avg {:?}, max {:?}",
        avg_claim_time, max_claim_time
    );

    assert!(
        max_claim_time < Duration::from_millis(100),
        "Max claim time {:?} exceeds SLO of 10ms (with 100x tolerance)",
        max_claim_time
    );

    assert!(elapsed.as_secs() < 30, "Test took too long: {:?}", elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 24)]
async fn stress_queue_contention_claim_slo() {
    let store = Arc::new(InMemoryEventStore::new());

    for i in 0..1000 {
        store.enqueue("test-queue", &format!("item-{}", i), "data");
    }

    let barrier = Arc::new(Barrier::new(20));
    let claim_times = Arc::new(std::sync::Mutex::new(Vec::new()));

    let handles: Vec<_> = (0..20)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let store = Arc::clone(&store);
            let times = Arc::clone(&claim_times);

            tokio::spawn(async move {
                barrier.wait().await;

                loop {
                    let claim_start = Instant::now();

                    let claim = store
                        .claim_next("test-queue", "consumer", Duration::from_secs(60))
                        .await
                        .unwrap();

                    times.lock().unwrap().push(claim_start.elapsed());

                    if claim.is_none() {
                        return;
                    }

                    if let Some(claim) = claim {
                        let _ = store.ack_claim(&claim.claim_id).await;
                    }
                }
            })
        })
        .collect();

    let _: Vec<_> = futures::future::join_all(handles).await;

    let times = claim_times.lock().unwrap();
    let p50_claim_time = {
        let mut sorted: Vec<_> = times.iter().copied().collect();
        sorted.sort();
        sorted[sorted.len() / 2]
    };

    println!(
        "Queue SLO test: {} claims, p50 latency = {:?}",
        times.len(),
        p50_claim_time
    );

    assert!(
        p50_claim_time < Duration::from_millis(10),
        "P50 claim latency {:?} exceeds SLO of 10ms",
        p50_claim_time
    );
}
