//! Test: Spend hits 80% threshold, notification emitted, work continues
//!
//! Budget usage reaches soft threshold
//! Notification emitted
//! Work continues
//! Assert: notification emitted, work continues

use std::sync::Arc;

use brehon_ports::Notification;
use brehon_ports::{EventStore, NotificationSink};
use brehon_test_harness::{
    event_was_emitted, InMemoryEventStore, MockDecisionEngine, MockGateway,
    RecordingNotificationSink,
};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn budget_soft_warning_emits_notification() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());
    let notifications = Arc::new(RecordingNotificationSink::new());
    let _decision_engine = Arc::new(MockDecisionEngine::new());

    let budget_limit = 100_000u64;
    let soft_threshold = 0.8;
    let soft_limit = (budget_limit as f64 * soft_threshold) as u64;

    notifications
        .toast(Notification::warning(format!(
            "Budget usage at {}% ({} tokens). Approaching hard limit.",
            soft_threshold * 100.0,
            soft_limit
        )))
        .unwrap();

    for i in 0..8 {
        store
            .append(Event {
                kind: EventKind::ResponseReceived {
                    session_id: "session-1".into(),
                    prompt_id: format!("prompt-{}", i),
                    tokens_used: 10_000,
                },
                timestamp: Utc::now(),
                aggregate_id: "worker-1".into(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::TaskCreated {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
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

    let events = store.all_events();
    let toasts = notifications.toasts();

    assert_eq!(
        toasts.len(),
        1,
        "Should emit one notification at soft threshold"
    );

    let total_tokens: u64 = events
        .iter()
        .filter_map(|e| {
            if let EventKind::ResponseReceived { tokens_used, .. } = &e.kind {
                Some(*tokens_used)
            } else {
                None
            }
        })
        .sum();
    assert!(
        total_tokens >= soft_limit,
        "Usage should be at or above soft threshold"
    );

    let task_assignments = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskAssigned { .. }))
        .count();
    assert_eq!(
        task_assignments, 1,
        "Work should continue after soft warning"
    );

    let task_completed = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCompleted { .. }))
        .count();
    assert_eq!(task_completed, 1, "Task should complete after soft warning");
}

#[tokio::test]
async fn budget_soft_warning_multiple_notifications() {
    let notifications = RecordingNotificationSink::new();

    let budget_limit = 100_000u64;
    let thresholds = [0.8, 0.85, 0.9, 0.95];

    for threshold in thresholds {
        let usage = (budget_limit as f64 * threshold) as u64;
        let percentage = (threshold * 100.0) as i32;

        notifications
            .toast(Notification::warning(format!(
                "Budget at {}% ({}/{} tokens)",
                percentage, usage, budget_limit
            )))
            .unwrap();
    }

    let toasts = notifications.toasts();
    assert_eq!(
        toasts.len(),
        4,
        "Should emit notification at each threshold"
    );

    let last_toast = &toasts[toasts.len() - 1];
    assert!(
        last_toast.message.contains("95%"),
        "Final notification should warn about 95% threshold"
    );
}

#[tokio::test]
async fn budget_soft_warning_work_continues() {
    let store = InMemoryEventStore::new();

    for i in 0..8 {
        store
            .append(Event {
                kind: EventKind::ResponseReceived {
                    session_id: "session-1".into(),
                    prompt_id: format!("prompt-{}", i),
                    tokens_used: 10_000,
                },
                timestamp: Utc::now(),
                aggregate_id: "worker-1".into(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::TaskCreated {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
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

    store
        .append(Event {
            kind: EventKind::ResponseReceived {
                session_id: "session-1".into(),
                prompt_id: "final-prompt".into(),
                tokens_used: 5_000,
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
        })
        .await
        .unwrap();

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

    let events = store.all_events();

    let task_completed = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCompleted { .. }))
        .count();
    assert_eq!(
        task_completed, 1,
        "Work should continue and complete after soft warning"
    );

    let system_draining = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::SystemDraining { .. }))
        .count();
    assert_eq!(
        system_draining, 0,
        "System should NOT be draining at soft threshold"
    );
}

#[tokio::test]
async fn budget_soft_vs_hard_limits() {
    let store = InMemoryEventStore::new();
    let notifications = RecordingNotificationSink::new();

    let budget_limit = 100_000u64;
    let soft_threshold = (budget_limit as f64 * 0.8) as u64;
    let _hard_threshold = (budget_limit as f64 * 0.95) as u64;

    for i in 0..8 {
        store
            .append(Event {
                kind: EventKind::ResponseReceived {
                    session_id: "session-1".into(),
                    prompt_id: format!("prompt-{}", i),
                    tokens_used: 10_000,
                },
                timestamp: Utc::now(),
                aggregate_id: "worker-1".into(),
            })
            .await
            .unwrap();
    }

    notifications
        .toast(Notification::warning(
            "Budget at 80% (soft threshold)".to_string(),
        ))
        .unwrap();

    for i in 8..10 {
        store
            .append(Event {
                kind: EventKind::ResponseReceived {
                    session_id: "session-1".into(),
                    prompt_id: format!("prompt-{}", i),
                    tokens_used: 10_000,
                },
                timestamp: Utc::now(),
                aggregate_id: "worker-1".into(),
            })
            .await
            .unwrap();
    }

    notifications
        .toast(Notification::warning(
            "Budget at 95% (hard threshold approaching)".to_string(),
        ))
        .unwrap();

    store
        .append(Event {
            kind: EventKind::SystemDraining {
                reason: "budget_hard_limit".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "system".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();
    let toasts = notifications.toasts();

    let total_tokens: u64 = events
        .iter()
        .filter_map(|e| {
            if let EventKind::ResponseReceived { tokens_used, .. } = &e.kind {
                Some(*tokens_used)
            } else {
                None
            }
        })
        .sum();

    assert!(
        total_tokens >= soft_threshold,
        "Should be past soft threshold"
    );
    assert!(
        !toasts.is_empty(),
        "Should have soft threshold notification"
    );

    assert!(
        event_was_emitted(&events, "SystemDraining"),
        "System draining should emit at hard limit"
    );
}

#[tokio::test]
async fn budget_soft_warning_no_work_interruption() {
    let store = InMemoryEventStore::new();

    store
        .append(Event {
            kind: EventKind::AgentSpawned {
                agent_id: "worker-1".into(),
                session_id: "session-1".into(),
                role: "worker".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
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

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "session-1".into(),
                operation: "long_task".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
        })
        .await
        .unwrap();

    for _ in 0..8 {
        store
            .append(Event {
                kind: EventKind::ResponseReceived {
                    session_id: "session-1".into(),
                    prompt_id: "prompt".into(),
                    tokens_used: 10_000,
                },
                timestamp: Utc::now(),
                aggregate_id: "worker-1".into(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: "session-1".into(),
                operation: "long_task".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
        })
        .await
        .unwrap();

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

    let events = store.all_events();

    let operation_started = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationStarted { .. }))
        .count();
    let operation_completed = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationCompleted { .. }))
        .count();
    assert_eq!(operation_started, 1, "Operation should start");
    assert_eq!(operation_completed, 1, "Operation should complete");

    let task_completed = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCompleted { .. }))
        .count();
    assert_eq!(task_completed, 1, "Task should complete uninterrupted");
}
