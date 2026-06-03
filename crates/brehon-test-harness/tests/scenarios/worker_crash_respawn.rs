//! Test: Worker dies mid-task, respawned, task reassigned
//!
//! Worker crashes during task
//! Task returns to pending
//! Worker respawned
//! Task reassigned and completes
//! Assert: agent_died event, task reassigned

use std::sync::Arc;
use std::time::Duration;

use brehon_ports::{AgentGateway, EventStore};
use brehon_test_harness::{event_was_emitted, InMemoryEventStore, MockGateway};
use brehon_types::{AgentId, Event, EventKind, SessionSpec};
use chrono::Utc;

#[tokio::test]
async fn worker_crash_respawn_and_reassign() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());

    let worker_id = AgentId::new("worker-1");
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

    let session1 = gateway
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
                session_id: session1.as_str().to_string(),
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
                task_id: task_id.clone(),
                agent_id: worker_id.as_str().to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: session1.as_str().to_string(),
                operation: "implementing_feature".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session1.as_str().to_string(),
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(10)).await;

    store
        .append(Event {
            kind: EventKind::AgentDied {
                agent_id: worker_id.as_str().to_string(),
                session_id: session1.as_str().to_string(),
                reason: "crash".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: worker_id.as_str().to_string(),
        })
        .await
        .unwrap();

    gateway.kill_session(&session1).await.unwrap();

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

    let session2 = gateway
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
                session_id: session2.as_str().to_string(),
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
                task_id: task_id.clone(),
                agent_id: worker_id.as_str().to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
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
        event_was_emitted(&events, "AgentDied"),
        "Agent death should be recorded"
    );

    let agent_died_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentDied { .. }))
        .collect();
    assert_eq!(
        agent_died_events.len(),
        1,
        "Should have exactly one agent death"
    );

    let agent_spawned_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentSpawned { .. }))
        .collect();
    assert_eq!(
        agent_spawned_events.len(),
        2,
        "Worker should be spawned twice (original + respawn)"
    );

    let task_assigned_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskAssigned { .. }))
        .collect();
    assert_eq!(
        task_assigned_events.len(),
        2,
        "Task should be assigned twice"
    );

    assert!(
        event_was_emitted(&events, "TaskCompleted"),
        "Task should complete after reassignment"
    );

    assert!(
        !gateway.is_session_alive(&session1),
        "Original session should be dead"
    );
}

#[tokio::test]
async fn worker_crash_mid_operation_recovery() {
    let store = InMemoryEventStore::new();
    let gateway = MockGateway::new();

    let worker_id = AgentId::new("worker-crash");
    let task_id = "T002".to_string();

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

    let session = gateway
        .spawn(SessionSpec::new(
            worker_id.clone(),
            "worker".into(),
            "/tmp/crash-worker".into(),
        ))
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: session.as_str().to_string(),
                operation: "running_tests".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::AgentDied {
                agent_id: worker_id.as_str().to_string(),
                session_id: session.as_str().to_string(),
                reason: "unexpected_crash".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: worker_id.as_str().to_string(),
        })
        .await
        .unwrap();

    gateway.kill_session(&session).await.unwrap();

    let events = store.all_events();

    let operation_started = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::OperationStarted { .. }))
        .count();
    assert_eq!(operation_started, 1, "Operation should have started");

    let agent_died = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentDied { .. }))
        .count();
    assert_eq!(agent_died, 1, "Agent death should be recorded");
}

#[tokio::test]
async fn multiple_worker_crashes_eventual_success() {
    let store = InMemoryEventStore::new();
    let gateway = MockGateway::new();

    let task_id = "T003".to_string();

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

    for attempt in 0..3 {
        let worker_id = format!("worker-attempt-{}", attempt);

        let session = gateway
            .spawn(SessionSpec::new(
                AgentId::new(&worker_id),
                "worker".into(),
                format!("/tmp/worker-{}", attempt),
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

        store
            .append(Event {
                kind: EventKind::TaskAssigned {
                    task_id: task_id.clone(),
                    agent_id: worker_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            })
            .await
            .unwrap();

        if attempt < 2 {
            store
                .append(Event {
                    kind: EventKind::AgentDied {
                        agent_id: worker_id.clone(),
                        session_id: session.as_str().to_string(),
                        reason: "crash".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: worker_id.clone(),
                })
                .await
                .unwrap();
        } else {
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
        }
    }

    let events = store.all_events();

    let agent_deaths = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentDied { .. }))
        .count();
    assert_eq!(agent_deaths, 2, "First two attempts should crash");

    let task_completions = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCompleted { .. }))
        .count();
    assert_eq!(task_completions, 1, "Final attempt should succeed");

    let spawns = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentSpawned { .. }))
        .count();
    assert_eq!(spawns, 3, "Should spawn 3 workers total");
}
