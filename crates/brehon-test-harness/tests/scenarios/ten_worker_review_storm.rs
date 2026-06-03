//! Test: 10 workers complete near-simultaneously, review queue respects lane priority and FIFO
//!
//! 10 workers complete tasks
//! Review queue processes in correct order
//! Priority lanes respected
//! FIFO within lane
//! Assert: correct review order

use std::sync::Arc;
use std::time::Duration;

use brehon_ports::{AgentGateway, EventStore};
use brehon_test_harness::{InMemoryEventStore, MockGateway};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn ten_worker_review_storm_priority_order() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());

    let workers: Vec<String> = (0..10).map(|i| format!("worker-{}", i)).collect();
    let tasks: Vec<String> = (0..10).map(|i| format!("T{:03}", i)).collect();
    let priorities = [
        "critical", "high", "high", "medium", "medium", "medium", "low", "low", "low", "low",
    ];

    for worker_id in &workers {
        let session = gateway
            .spawn(brehon_types::SessionSpec::new(
                brehon_types::AgentId::new(worker_id),
                "worker".into(),
                format!("/tmp/{}", worker_id),
            ))
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::AgentSpawned {
                    agent_id: worker_id.clone(),
                    session_id: session.as_str().to_string(),
                    role: "worker".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: worker_id.clone(),
            })
            .await
            .unwrap();
    }

    for (idx, task_id) in tasks.iter().enumerate() {
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

        store
            .append(Event {
                kind: EventKind::TaskAssigned {
                    task_id: task_id.clone(),
                    agent_id: workers[idx].clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            })
            .await
            .unwrap();
    }

    tokio::time::sleep(Duration::from_millis(10)).await;

    for (idx, task_id) in tasks.iter().enumerate() {
        store
            .append(Event {
                kind: EventKind::TaskCompleted {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            })
            .await
            .unwrap();

        let priority = priorities[idx];
        let lane = format!("review:{}", priority);
        store.enqueue(&lane, task_id, priority);
    }

    let mut review_order: Vec<String> = vec![];

    for lane in [
        "review:critical",
        "review:high",
        "review:medium",
        "review:low",
    ] {
        while let Some(claim) = store
            .claim_next(lane, "reviewer", Duration::from_secs(60))
            .await
            .unwrap()
        {
            review_order.push(claim.item_id.clone());
            store.ack_claim(&claim.claim_id).await.unwrap();
        }
    }

    let events = store.all_events();

    assert_eq!(
        review_order.len(),
        10,
        "All 10 tasks should be queued for review"
    );

    let critical_pos: Vec<usize> = review_order
        .iter()
        .enumerate()
        .filter(|(_, id)| *id == "T000")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        critical_pos.len(),
        1,
        "Critical task should be in review order"
    );
    assert_eq!(critical_pos[0], 0, "Critical task (T000) should be first");

    let high_positions: Vec<usize> = ["T001", "T002"]
        .iter()
        .filter_map(|t| review_order.iter().position(|id| id == *t))
        .collect();
    assert_eq!(
        high_positions.len(),
        2,
        "Both high priority tasks should be queued"
    );
    assert!(
        high_positions.iter().all(|&p| (1..3).contains(&p)),
        "High priority tasks should be positions 1-2"
    );

    let medium_positions: Vec<usize> = ["T003", "T004", "T005"]
        .iter()
        .filter_map(|t| review_order.iter().position(|id| id == *t))
        .collect();
    assert_eq!(
        medium_positions.len(),
        3,
        "All medium priority tasks should be queued"
    );
    assert!(
        medium_positions.iter().all(|&p| (3..6).contains(&p)),
        "Medium priority tasks should be positions 3-5"
    );

    let low_positions: Vec<usize> = ["T006", "T007", "T008", "T009"]
        .iter()
        .filter_map(|t| review_order.iter().position(|id| id == *t))
        .collect();
    assert_eq!(
        low_positions.len(),
        4,
        "All low priority tasks should be queued"
    );
    assert!(
        low_positions.iter().all(|&p| p >= 6),
        "Low priority tasks should be positions 6+"
    );

    let task_completions = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCompleted { .. }))
        .count();
    assert_eq!(
        task_completions, 10,
        "All 10 workers should complete their tasks"
    );

    let agent_spawns = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentSpawned { .. }))
        .count();
    assert_eq!(agent_spawns, 10, "Should spawn 10 worker agents");
}

#[tokio::test]
async fn review_queue_fifo_within_priority_lane() {
    let store = InMemoryEventStore::new();

    let high_priority_tasks = vec!["T001", "T002", "T003"];

    for task_id in &high_priority_tasks {
        store.enqueue("review:high", task_id, "high");
    }

    let mut order: Vec<String> = vec![];
    while let Some(claim) = store
        .claim_next("review:high", "reviewer", Duration::from_secs(60))
        .await
        .unwrap()
    {
        order.push(claim.item_id.clone());
        store.ack_claim(&claim.claim_id).await.unwrap();
    }

    assert_eq!(
        order, high_priority_tasks,
        "Within same lane, FIFO order should be preserved"
    );

    for task_id in &high_priority_tasks {
        store
            .append(Event {
                kind: EventKind::TaskCompleted {
                    task_id: task_id.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .unwrap();
    }

    let events = store.all_events();
    let completions: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::TaskCompleted { task_id } = &e.kind {
                Some(task_id.clone())
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        completions.len(),
        3,
        "All high priority tasks should complete"
    );
}

#[tokio::test]
async fn review_queue_mixed_priorities_correct_order() {
    let store = InMemoryEventStore::new();

    let tasks = vec![
        ("T001", "medium"),
        ("T002", "high"),
        ("T003", "low"),
        ("T004", "critical"),
        ("T005", "medium"),
    ];

    for (task_id, priority) in &tasks {
        let lane = format!("review:{}", priority);
        store.enqueue(&lane, task_id, priority);
    }

    let mut processed: Vec<String> = vec![];

    for lane in [
        "review:critical",
        "review:high",
        "review:medium",
        "review:low",
    ] {
        while let Some(claim) = store
            .claim_next(lane, "reviewer", Duration::from_secs(60))
            .await
            .unwrap()
        {
            processed.push(claim.item_id.clone());
            store.ack_claim(&claim.claim_id).await.unwrap();
        }
    }

    assert_eq!(processed.len(), 5, "All tasks should be processed");
    assert_eq!(processed[0], "T004", "Critical task should be first");
    assert_eq!(processed[1], "T002", "High priority should be second");

    let medium_indices: Vec<usize> = processed
        .iter()
        .enumerate()
        .filter(|(_, id)| **id == "T001" || **id == "T005")
        .map(|(i, _)| i)
        .collect();
    assert!(
        medium_indices.iter().all(|&i| i > 1 && i < 4),
        "Medium tasks should be after high"
    );

    let low_index = processed.iter().position(|id| id == "T003").unwrap();
    assert!(low_index >= 4, "Low priority should be last");
}

#[tokio::test]
async fn ten_workers_concurrent_completions_no_lost_reviews() {
    let store = Arc::new(InMemoryEventStore::new());

    let mut handles = vec![];
    for i in 0..10u8 {
        let store = Arc::clone(&store);
        let handle = tokio::spawn(async move {
            let task_id = format!("T{:03}", i);
            let lane = if i < 2 {
                "review:critical"
            } else if i < 5 {
                "review:high"
            } else {
                "review:medium"
            };

            store.enqueue(lane, &task_id, "priority");

            store
                .append(Event {
                    kind: EventKind::TaskCompleted {
                        task_id: task_id.clone(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: task_id.clone(),
                })
                .await
                .unwrap();
        });
        handles.push(handle);
    }

    futures::future::join_all(handles).await;

    let events = store.all_events();
    let completions = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCompleted { .. }))
        .count();
    assert_eq!(completions, 10, "All 10 tasks should complete");

    let total_queued = store.queue_len("review:critical")
        + store.queue_len("review:high")
        + store.queue_len("review:medium");
    assert_eq!(total_queued, 10, "All 10 tasks should be queued");
}

#[tokio::test]
async fn review_queue_respects_claim_expiry() {
    let store = InMemoryEventStore::new();

    store.enqueue("review:high", "T001", "high");
    store.enqueue("review:high", "T002", "high");

    let claim1 = store
        .claim_next("review:high", "reviewer-1", Duration::from_millis(10))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(claim1.item_id, "T001", "First claim should be T001");

    tokio::time::sleep(Duration::from_millis(20)).await;

    let claim2_attempt = store
        .claim_next("review:high", "reviewer-2", Duration::from_secs(60))
        .await
        .unwrap();

    assert!(
        claim2_attempt.is_some(),
        "Should be able to claim after expiry"
    );
    if let Some(claim) = claim2_attempt {
        assert_eq!(claim.item_id, "T001", "Should reclaim T001 after expiry");
    }

    let claim3 = store
        .claim_next("review:high", "reviewer-3", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        claim3.item_id, "T002",
        "Next item should be T002 after T001 is claimed"
    );
}
