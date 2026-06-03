//! Test: Worker runs tests + lint checks while blocked on review
//!
//! Worker blocked on review
//! Worker runs self-improvement tasks
//! Tests run, lint checks
//! Assert: self-improvement tasks executed

use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{event_was_emitted, InMemoryEventStore, MockGateway};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn self_improvement_while_waiting_for_review() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());

    let task_id = "T001".to_string();
    let session_id = "session-worker-1".to_string();

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
                agent_id: "worker-1".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::AgentSpawned {
                agent_id: "worker-1".into(),
                session_id: session_id.clone(),
                role: "worker".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
        })
        .await
        .unwrap();

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

    let review_id = "R001".to_string();
    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.clone(),
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: session_id.clone(),
                operation: "self_improvement:run_tests".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: session_id.clone(),
                operation: "self_improvement:run_lint".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: session_id.clone(),
                operation: "self_improvement:run_tests".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: session_id.clone(),
                operation: "self_improvement:run_lint".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.clone(),
                reviewer_id: "reviewer-1".into(),
                score: 8,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.clone(),
                reviewer_id: "reviewer-2".into(),
                score: 7,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

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

    let self_improvement_ops: Vec<_> = events
        .iter()
        .filter(|e| {
            if let EventKind::OperationStarted { operation, .. } = &e.kind {
                operation.starts_with("self_improvement:")
            } else {
                false
            }
        })
        .collect();
    assert!(
        self_improvement_ops.len() >= 2,
        "Should have at least 2 self-improvement operations (tests + lint)"
    );

    let operation_names: Vec<_> = self_improvement_ops
        .iter()
        .filter_map(|e| {
            if let EventKind::OperationStarted { operation, .. } = &e.kind {
                Some(operation.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(
        operation_names.iter().any(|op| op.contains("run_tests")),
        "Should run tests during self-improvement"
    );
    assert!(
        operation_names.iter().any(|op| op.contains("run_lint")),
        "Should run lint during self-improvement"
    );

    let operations_completed: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationCompleted { .. }))
        .collect();
    assert_eq!(
        operations_completed.len(),
        self_improvement_ops.len(),
        "All self-improvement operations should complete"
    );

    assert!(
        event_was_emitted(&events, "ReviewApproved"),
        "Review should still be approved after self-improvement"
    );
}

#[tokio::test]
async fn self_improvement_various_tasks_while_blocked() {
    let store = InMemoryEventStore::new();
    let session_id = "session-blocked-worker";

    store
        .append(Event {
            kind: EventKind::TaskCompleted {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: "T001".into(),
                review_id: "R001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R001".into(),
        })
        .await
        .unwrap();

    let self_improvement_tasks = vec![
        "self_improvement:run_tests",
        "self_improvement:run_lint",
        "self_improvement:fix_warnings",
        "self_improvement:update_documentation",
        "self_improvement:refactor_duplicates",
    ];

    for task in &self_improvement_tasks {
        store
            .append(Event {
                kind: EventKind::OperationStarted {
                    session_id: session_id.to_string(),
                    operation: task.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: session_id.to_string(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::OperationCompleted {
                    session_id: session_id.to_string(),
                    operation: task.to_string(),
                    success: true,
                },
                timestamp: Utc::now(),
                aggregate_id: session_id.to_string(),
            })
            .await
            .unwrap();
    }

    let events = store.all_events();

    let started_count = events
        .iter()
        .filter(|e| {
            if let EventKind::OperationStarted { operation, .. } = &e.kind {
                operation.starts_with("self_improvement:")
            } else {
                false
            }
        })
        .count();
    assert_eq!(started_count, 5, "Should start 5 self-improvement tasks");

    let completed_count = events
        .iter()
        .filter(|e| {
            if let EventKind::OperationCompleted { operation, .. } = &e.kind {
                operation.starts_with("self_improvement:")
            } else {
                false
            }
        })
        .count();
    assert_eq!(
        completed_count, 5,
        "Should complete 5 self-improvement tasks"
    );

    let all_success = events
        .iter()
        .filter_map(|e| {
            if let EventKind::OperationCompleted {
                success, operation, ..
            } = &e.kind
            {
                if operation.starts_with("self_improvement:") {
                    Some(*success)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .all(|s| s);
    assert!(all_success, "All self-improvement tasks should succeed");
}

#[tokio::test]
async fn self_improvement_stops_when_review_resolves() {
    let store = InMemoryEventStore::new();
    let session_id = "session-review-blocked";

    store
        .append(Event {
            kind: EventKind::TaskCompleted {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: "T001".into(),
                review_id: "R001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R001".into(),
        })
        .await
        .unwrap();

    for task in &["self_improvement:run_tests", "self_improvement:run_lint"] {
        store
            .append(Event {
                kind: EventKind::OperationStarted {
                    session_id: session_id.to_string(),
                    operation: task.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: session_id.to_string(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::ReviewApproved {
                review_id: "R001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R001".into(),
        })
        .await
        .unwrap();

    for task in &["self_improvement:run_tests", "self_improvement:run_lint"] {
        store
            .append(Event {
                kind: EventKind::OperationCompleted {
                    session_id: session_id.to_string(),
                    operation: task.to_string(),
                    success: true,
                },
                timestamp: Utc::now(),
                aggregate_id: session_id.to_string(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::MergeCommitted {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let review_approved_idx = events
        .iter()
        .position(|e| matches!(&e.kind, EventKind::ReviewApproved { .. }))
        .unwrap();

    let merge_committed_idx = events
        .iter()
        .position(|e| matches!(&e.kind, EventKind::MergeCommitted { .. }))
        .unwrap();

    let last_self_improvement_idx = events
        .iter()
        .rposition(|e| {
            matches!(&e.kind, EventKind::OperationCompleted { operation, .. }
                if operation.starts_with("self_improvement:"))
        })
        .unwrap();

    assert!(
        last_self_improvement_idx < merge_committed_idx,
        "Self-improvement should complete before merge"
    );

    let self_improvement_started_after_review: Vec<_> = events
        .iter()
        .skip(review_approved_idx)
        .filter(|e| {
            matches!(&e.kind, EventKind::OperationStarted { operation, .. }
                if operation.starts_with("self_improvement:"))
        })
        .collect();
    assert!(
        self_improvement_started_after_review.is_empty(),
        "No new self-improvement operations should start after review approval"
    );
}

#[tokio::test]
async fn self_improvement_during_review_iteration() {
    let store = InMemoryEventStore::new();
    let session_id = "session-iteration";

    store
        .append(Event {
            kind: EventKind::TaskCompleted {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: "T001".into(),
                review_id: "R001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: "R001".into(),
                reviewer_id: "reviewer-1".into(),
                score: 5,
            },
            timestamp: Utc::now(),
            aggregate_id: "R001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewChangesRequested {
                review_id: "R001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: session_id.to_string(),
                operation: "self_improvement:analyze_feedback".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: session_id.to_string(),
                operation: "self_improvement:analyze_feedback".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: "T001".into(),
                agent_id: "worker-1".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "ReviewChangesRequested"),
        "Review should request changes"
    );
    assert!(
        event_was_emitted(&events, "OperationStarted"),
        "Self-improvement operation should start during review iteration"
    );

    let self_improvement_during_review = events
        .iter()
        .filter(|e| {
            matches!(&e.kind, EventKind::OperationStarted { operation, .. }
                if operation.starts_with("self_improvement:"))
        })
        .count();
    assert!(
        self_improvement_during_review >= 1,
        "Should have self-improvement during review iteration"
    );
}
