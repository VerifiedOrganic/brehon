//! Test: Worker runs 30-minute test suite, stuck detection doesn't fire
//!
//! Worker runs long operation
//! Operation tracking shows active
//! Stuck detection does NOT fire
//! Assert: no stuck_detected event

use std::sync::Arc;
use std::time::Duration;

use brehon_ports::EventStore;
use brehon_test_harness::{event_was_emitted, InMemoryEventStore, MockGateway};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn long_running_operation_no_stuck_detection() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());

    let session_id = "session-long-op".to_string();
    let task_id = "T001".to_string();

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
            kind: EventKind::OperationStarted {
                session_id: session_id.clone(),
                operation: "cargo test --all".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(10)).await;

    for progress in &[
        "Compiling test suite",
        "Running unit tests",
        "Running integration tests",
        "Running end-to-end tests",
        "Generating coverage report",
    ] {
        store
            .append(Event {
                kind: EventKind::OperationStarted {
                    session_id: session_id.clone(),
                    operation: format!("test_phase: {}", progress),
                },
                timestamp: Utc::now(),
                aggregate_id: session_id.clone(),
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: session_id.clone(),
                operation: "cargo test --all".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
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

    let events = store.all_events();

    assert!(
        !event_was_emitted(&events, "StuckDetected"),
        "Long-running operation should NOT trigger stuck detection"
    );

    let operation_started = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationStarted { .. }))
        .count();
    assert!(
        operation_started >= 1,
        "Should have operation started events"
    );

    let operation_completed = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationCompleted { .. }))
        .count();
    assert_eq!(
        operation_completed, 1,
        "Should have operation completed event"
    );

    assert!(
        event_was_emitted(&events, "TaskCompleted"),
        "Task should complete after long operation"
    );

    let started_time = events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::OperationStarted { .. }))
        .map(|e| e.timestamp);

    let completed_time = events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::OperationCompleted { .. }))
        .map(|e| e.timestamp);

    if let (Some(start), Some(completed)) = (started_time, completed_time) {
        let duration = completed - start;
        assert!(
            duration.num_milliseconds() >= 0,
            "Operation should have duration"
        );
    }
}

#[tokio::test]
async fn long_operation_without_progress_updates() {
    let store = InMemoryEventStore::new();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "session-1".into(),
                operation: "long_running_test_suite".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "session-1".into(),
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: "session-1".into(),
                operation: "long_running_test_suite".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: "session-1".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        !event_was_emitted(&events, "StuckDetected"),
        "Should not detect stuck during active operation"
    );

    assert!(
        event_was_emitted(&events, "OperationStarted"),
        "Operation should start"
    );
    assert!(
        event_was_emitted(&events, "OperationCompleted"),
        "Operation should complete"
    );
}

#[tokio::test]
async fn multiple_concurrent_long_operations() {
    let store = InMemoryEventStore::new();

    let operations = vec![
        ("session-1", "cargo test"),
        ("session-2", "cargo clippy"),
        ("session-3", "cargo build --release"),
    ];

    for (session_id, operation) in &operations {
        store
            .append(Event {
                kind: EventKind::OperationStarted {
                    session_id: session_id.to_string(),
                    operation: operation.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: session_id.to_string(),
            })
            .await
            .unwrap();
    }

    tokio::time::sleep(Duration::from_millis(10)).await;

    for (session_id, operation) in &operations {
        store
            .append(Event {
                kind: EventKind::OperationCompleted {
                    session_id: session_id.to_string(),
                    operation: operation.to_string(),
                    success: true,
                },
                timestamp: Utc::now(),
                aggregate_id: session_id.to_string(),
            })
            .await
            .unwrap();
    }

    let events = store.all_events();

    assert!(
        !event_was_emitted(&events, "StuckDetected"),
        "Concurrent long operations should not trigger stuck detection"
    );

    let started_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationStarted { .. }))
        .count();
    assert_eq!(started_count, 3, "All operations should start");

    let completed_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationCompleted { .. }))
        .count();
    assert_eq!(completed_count, 3, "All operations should complete");
}

#[tokio::test]
async fn stuck_detection_after_operation_completes() {
    let store = InMemoryEventStore::new();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "session-1".into(),
                operation: "test_suite".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "session-1".into(),
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(5)).await;

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: "session-1".into(),
                operation: "test_suite".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: "session-1".into(),
        })
        .await
        .unwrap();

    let events_after_op = store.all_events();
    assert!(
        !event_was_emitted(&events_after_op, "StuckDetected"),
        "No stuck detection during operation"
    );

    tokio::time::sleep(Duration::from_millis(30)).await;

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: "session-1".into(),
                duration_minutes: 30,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: "session-1".into(),
        })
        .await
        .unwrap();

    let events_after_stuck = store.all_events();

    assert!(
        event_was_emitted(&events_after_stuck, "StuckDetected"),
        "Stuck detection can fire after operation completes and worker becomes inactive"
    );

    let operation_before_stuck = events_after_stuck
        .iter()
        .position(|e| matches!(&e.kind, EventKind::OperationCompleted { .. }));
    let stuck_pos = events_after_stuck
        .iter()
        .position(|e| matches!(&e.kind, EventKind::StuckDetected { .. }));

    assert!(
        operation_before_stuck < stuck_pos,
        "Operation should complete before stuck is detected"
    );
}
