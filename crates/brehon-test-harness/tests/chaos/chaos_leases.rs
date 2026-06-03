//! Chaos test: Random lease expiry / slow consumer.
//!
//! Verifies no double-claiming occurs under chaotic lease conditions.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_ports::EventStore;
use brehon_test_harness::{ChaosConfig, ChaosInjector, InMemoryEventStore};

#[tokio::test]
async fn chaos_lease_no_double_claiming() {
    let store = Arc::new(InMemoryEventStore::new());

    let item_count = 10;
    for i in 0..item_count {
        store.enqueue("work", &format!("item-{}", i), "data");
    }

    let consumer_count = 3;
    let claims = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let processed = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    for consumer_id in 0..consumer_count {
        let store = Arc::clone(&store);
        let claims = Arc::clone(&claims);
        let processed = Arc::clone(&processed);

        handles.push(tokio::spawn(async move {
            let consumer_name = format!("consumer-{}", consumer_id);

            loop {
                let claim_result = store
                    .claim_next("work", &consumer_name, Duration::from_secs(60))
                    .await;

                match claim_result {
                    Ok(Some(claim)) => {
                        {
                            let mut claims_guard = claims.lock().unwrap();
                            if claims_guard.contains(&claim.item_id) {
                                panic!("Double claim detected: {}", claim.item_id);
                            }
                            claims_guard.insert(claim.item_id.clone());
                        }

                        tokio::time::sleep(Duration::from_millis(5)).await;

                        match store.ack_claim(&claim.claim_id).await {
                            Ok(()) => {
                                processed.fetch_add(1, Ordering::SeqCst);
                            }
                            Err(_) => {
                                let mut claims_guard = claims.lock().unwrap();
                                claims_guard.remove(&claim.item_id);
                            }
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
    assert_eq!(final_processed, item_count, "Not all items processed");
}

#[tokio::test]
async fn chaos_lease_expiry_recovery() {
    let store = Arc::new(InMemoryEventStore::new());

    store.enqueue("work", "item-1", "data");
    store.enqueue("work", "item-2", "data");

    let claim1 = store
        .claim_next("work", "consumer-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim1.item_id, "item-1");

    store.ack_claim(&claim1.claim_id).await.unwrap();
    assert_eq!(store.queue_len("work"), 1, "One item left after ack");

    let claim2 = store
        .claim_next("work", "consumer-2", Duration::from_millis(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim2.item_id, "item-2");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let claim3 = store
        .claim_next("work", "consumer-3", Duration::from_secs(60))
        .await
        .unwrap();

    assert!(
        claim3.is_some(),
        "Should reclaim item-2 after lease expiry cleanup in claim_next"
    );
    assert_eq!(claim3.unwrap().item_id, "item-2");
}

#[tokio::test]
async fn chaos_lease_slow_consumer() {
    let config = ChaosConfig::with_lease_expiry(0.1);
    let store = Arc::new(InMemoryEventStore::new());

    let item_count = 10;
    for i in 0..item_count {
        store.enqueue("work", &format!("item-{}", i), "data");
    }

    let claimed_items = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let total_claims = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    for consumer_id in 0..3 {
        let store = Arc::clone(&store);
        let claimed_items = Arc::clone(&claimed_items);
        let total_claims = Arc::clone(&total_claims);
        let config = config.clone();

        handles.push(tokio::spawn(async move {
            let consumer_name = format!("consumer-{}", consumer_id);
            let mut injector = ChaosInjector::new(config);

            for _ in 0..20 {
                let lease_for = Duration::from_secs(60);

                if let Ok(Some(claim)) = store.claim_next("work", &consumer_name, lease_for).await {
                    total_claims.fetch_add(1, Ordering::SeqCst);

                    {
                        let mut items = claimed_items.lock().unwrap();
                        if !items.insert(claim.item_id.clone()) {
                            panic!("Double claim detected for {}", claim.item_id);
                        }
                    }

                    let processing_time = if injector.should_expire_lease() {
                        Duration::from_millis(1)
                    } else {
                        Duration::from_millis(50)
                    };

                    tokio::time::sleep(processing_time).await;

                    let _ = store.ack_claim(&claim.claim_id).await;
                }

                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }));
    }

    futures::future::join_all(handles).await;

    let processed = claimed_items.lock().unwrap().len();
    assert_eq!(processed, item_count, "All items should be claimed once");

    assert_eq!(store.queue_len("work"), 0, "Queue should be empty");
}
