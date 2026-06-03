//! Test: Total spend hits limit, all work drains
//!
//! Budget usage reaches hard limit
//! SystemDraining event emitted
//! All work stops
//! Assert: system_draining event, work stops

use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{event_was_emitted, InMemoryEventStore, MockDecisionEngine, MockGateway};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn budget_hard_limit_triggers_draining() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());
    let _decision_engine = Arc::new(MockDecisionEngine::new());

    let budget_limit = 100_000u64;
    let _current_usage = 100_000u64;

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

    for i in 0..10 {
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
            kind: EventKind::SystemDraining {
                reason: "budget_hard_limit_reached".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "system".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "SystemDraining"),
        "SystemDraining event should be emitted when hard limit is reached"
    );

    let draining_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::SystemDraining { .. }))
        .collect();
    assert_eq!(
        draining_events.len(),
        1,
        "Should have exactly one SystemDraining event"
    );

    if let EventKind::SystemDraining { reason } = &draining_events[0].kind {
        assert!(
            reason.contains("budget") || reason.contains("limit"),
            "Draining reason should mention budget or limit"
        );
    }

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
        total_tokens >= budget_limit,
        "Total tokens used should meet or exceed hard limit"
    );

    let new_tasks_after_draining = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCreated { .. }))
        .count();
    assert_eq!(
        new_tasks_after_draining, 1,
        "No new tasks should be created after draining starts"
    );

    let task_assignments_after_draining = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskAssigned { .. }))
        .count();
    assert_eq!(
        task_assignments_after_draining, 1,
        "No new task assignments should occur after draining"
    );
}

#[tokio::test]
async fn budget_hard_limit_stops_new_work() {
    let store = InMemoryEventStore::new();

    store
        .append(Event {
            kind: EventKind::ResponseReceived {
                session_id: "session-1".into(),
                prompt_id: "p1".into(),
                tokens_used: 50_000,
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ResponseReceived {
                session_id: "session-1".into(),
                prompt_id: "p2".into(),
                tokens_used: 50_000,
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::SystemDraining {
                reason: "budget_exhausted".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "system".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();

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
    assert_eq!(total_tokens, 100_000, "Budget should be exhausted");

    assert!(
        event_was_emitted(&events, "SystemDraining"),
        "System draining should be triggered"
    );
}

#[tokio::test]
async fn budget_hard_limit_allows_active_work_to_complete() {
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
            kind: EventKind::AgentSpawned {
                agent_id: "reviewer-1".into(),
                session_id: "session-2".into(),
                role: "reviewer".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "reviewer-1".into(),
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
            kind: EventKind::SystemDraining {
                reason: "budget_hard_limit".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "system".into(),
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

    store
        .append(Event {
            kind: EventKind::TaskCreated {
                task_id: "T002".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T002".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::AgentDied {
                agent_id: "worker-1".into(),
                session_id: "session-1".into(),
                reason: "system_draining".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-1".into(),
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
        "Active task should complete before draining"
    );

    let new_task_after_draining = events
        .iter()
        .filter(|e| {
            if let EventKind::TaskCreated { task_id } = &e.kind {
                task_id == "T002"
            } else {
                false
            }
        })
        .count();
    assert_eq!(
        new_task_after_draining, 1,
        "New task created but should not be assigned"
    );

    let new_task_assignments = events
        .iter()
        .filter(|e| {
            if let EventKind::TaskAssigned { task_id, .. } = &e.kind {
                task_id == "T002"
            } else {
                false
            }
        })
        .count();
    assert_eq!(
        new_task_assignments, 0,
        "New task should not be assigned after draining starts"
    );
}
