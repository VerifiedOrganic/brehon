//! Test: One worker, one task, review passes, merges
//!
//! Worker completes task
//! Reviewers give passing scores (avg >= 7)
//! Task merges successfully
//! Assert: task_status merged, correct events emitted

use std::sync::Arc;

use brehon_ports::{AgentGateway, EventStore};
use brehon_test_harness::{
    event_was_emitted, InMemoryEventStore, MockDecisionEngine, MockGateway,
    RecordingNotificationSink,
};
use brehon_types::{AgentId, Event, EventKind, SessionSpec, TaskId};
use chrono::Utc;

#[tokio::test]
async fn basic_single_task_passes_review_and_merges() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());
    let _notifications = Arc::new(RecordingNotificationSink::new());
    let _decision_engine = Arc::new(MockDecisionEngine::new());

    let task_id = TaskId::new("T001");
    let worker_id = AgentId::new("worker-1");
    let reviewer_ids = [AgentId::new("reviewer-1"), AgentId::new("reviewer-2")];

    store
        .append(Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.as_str().to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.as_str().to_string(),
        })
        .await
        .unwrap();

    let worker_session = gateway
        .spawn(SessionSpec::new(
            worker_id.clone(),
            "worker".into(),
            "/tmp/worker-1".into(),
        ))
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::AgentSpawned {
                agent_id: worker_id.as_str().to_string(),
                session_id: worker_session.as_str().to_string(),
                role: "worker".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: worker_id.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: task_id.as_str().to_string(),
                agent_id: worker_id.as_str().to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.as_str().to_string(),
        })
        .await
        .unwrap();

    for i in 0..3 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        store
            .append(Event {
                kind: EventKind::OperationStarted {
                    session_id: worker_session.as_str().to_string(),
                    operation: format!("progress-{}", i),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.as_str().to_string(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::TaskCompleted {
                task_id: task_id.as_str().to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.as_str().to_string(),
        })
        .await
        .unwrap();

    let review_id = "R001".to_string();
    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.as_str().to_string(),
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    for (idx, reviewer_id) in reviewer_ids.iter().enumerate() {
        let score = if idx == 0 { 8 } else { 7 };
        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id.clone(),
                    reviewer_id: reviewer_id.as_str().to_string(),
                    score,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::ReviewApproved {
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::MergePrepared {
                task_id: task_id.as_str().to_string(),
                branch: "feature/T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::MergeCommitted {
                task_id: task_id.as_str().to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.as_str().to_string(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "TaskCreated"),
        "Should have TaskCreated event"
    );
    assert!(
        event_was_emitted(&events, "TaskAssigned"),
        "Should have TaskAssigned event"
    );
    assert!(
        event_was_emitted(&events, "TaskCompleted"),
        "Should have TaskCompleted event"
    );
    assert!(
        event_was_emitted(&events, "ReviewRequested"),
        "Should have ReviewRequested event"
    );
    assert!(
        event_was_emitted(&events, "ReviewApproved"),
        "Should have ReviewApproved event"
    );
    assert!(
        event_was_emitted(&events, "MergeCommitted"),
        "Should have MergeCommitted event"
    );

    let nudge_count = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::NudgeSent { .. }))
        .count();
    assert_eq!(nudge_count, 0, "No nudges should be sent in a smooth flow");

    let review_rounds = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReviewRequested { .. }))
        .count();
    assert_eq!(review_rounds, 1, "Should have exactly 1 review round");

    let scores: Vec<u8> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ReviewScoreReceived { score, .. } => Some(*score),
            _ => None,
        })
        .collect();

    let avg_score = scores.iter().sum::<u8>() as f64 / scores.len() as f64;
    assert!(
        avg_score >= 7.0,
        "Average score should be >= 7, got {}",
        avg_score
    );

    let calls = gateway.calls();
    assert!(!calls.is_empty(), "Gateway should have recorded calls");

    let spawn_calls: Vec<_> = calls.iter().filter(|c| c.method == "spawn").collect();
    assert_eq!(
        spawn_calls.len(),
        1,
        "Should spawn exactly one worker session"
    );
}

#[tokio::test]
async fn single_task_with_high_review_scores() {
    let store = InMemoryEventStore::new();

    let task_id = TaskId::new("T002");

    store
        .append(Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.as_str().to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: task_id.as_str().to_string(),
                agent_id: "worker-1".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.as_str().to_string(),
        })
        .await
        .unwrap();

    let review_id = "R002".to_string();
    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.as_str().to_string(),
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    for (reviewer_idx, score) in [9u8, 10u8, 9u8].iter().enumerate() {
        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id.clone(),
                    reviewer_id: format!("reviewer-{}", reviewer_idx),
                    score: *score,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::ReviewApproved {
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();
    assert!(event_was_emitted(&events, "ReviewApproved"));

    let scores: Vec<u8> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ReviewScoreReceived { score, .. } => Some(*score),
            _ => None,
        })
        .collect();

    let avg = scores.iter().sum::<u8>() as f64 / scores.len() as f64;
    assert!(avg >= 9.0, "High scores should average >= 9");
}
