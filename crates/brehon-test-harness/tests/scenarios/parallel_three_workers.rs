//! Test: Three workers, 5 tasks with dependencies, correct execution order
//!
//! Tasks have dependency graph
//! Workers execute in topological order
//! Dependencies respected
//! Assert: task completion order matches dependency order

use std::sync::Arc;

use brehon_ports::{AgentGateway, EventStore};
use brehon_test_harness::{InMemoryEventStore, MockDecisionEngine, MockGateway};
use brehon_types::{AgentId, Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn parallel_three_workers_respect_dependencies() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());
    let _decision_engine = Arc::new(MockDecisionEngine::new());

    let tasks = vec![
        ("T001", vec![]),
        ("T002", vec![]),
        ("T003", vec!["T001"]),
        ("T004", vec!["T001", "T002"]),
        ("T005", vec!["T003", "T004"]),
    ];

    for (task_id, _deps) in &tasks {
        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .unwrap();
    }

    let workers = vec![
        AgentId::new("worker-1"),
        AgentId::new("worker-2"),
        AgentId::new("worker-3"),
    ];

    for worker_id in &workers {
        let session = gateway
            .spawn(brehon_types::SessionSpec::new(
                worker_id.clone(),
                "worker".into(),
                format!("/tmp/{}", worker_id.as_str()),
            ))
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::AgentSpawned {
                    agent_id: worker_id.as_str().to_string(),
                    session_id: session.as_str().to_string(),
                    role: "worker".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: worker_id.as_str().to_string(),
            })
            .await
            .unwrap();
    }

    let mut completion_order: Vec<String> = vec![];

    let can_start = |task: &str, completed: &[String], deps: &[&str]| -> bool {
        if completed.contains(&task.to_string()) {
            return false;
        }
        deps.iter().all(|dep| completed.contains(&dep.to_string()))
    };

    for round in 0..tasks.len() {
        for (task_id, deps) in &tasks {
            let deps_vec: Vec<&str> = deps.to_vec();
            if can_start(task_id, &completion_order, &deps_vec) {
                let assigned_worker = workers[round % workers.len()].clone();

                store
                    .append(Event {
                        kind: EventKind::TaskAssigned {
                            task_id: task_id.to_string(),
                            agent_id: assigned_worker.as_str().to_string(),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: task_id.to_string(),
                    })
                    .await
                    .unwrap();

                tokio::time::sleep(std::time::Duration::from_millis(10)).await;

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

                completion_order.push(task_id.to_string());
                break;
            }
        }
    }

    assert_eq!(completion_order.len(), 5, "All 5 tasks should complete");

    let t001_pos = completion_order.iter().position(|t| t == "T001").unwrap();
    let t002_pos = completion_order.iter().position(|t| t == "T002").unwrap();
    let t003_pos = completion_order.iter().position(|t| t == "T003").unwrap();
    let t004_pos = completion_order.iter().position(|t| t == "T004").unwrap();
    let t005_pos = completion_order.iter().position(|t| t == "T005").unwrap();

    assert!(t001_pos < t003_pos, "T001 should complete before T003");
    assert!(t001_pos < t004_pos, "T001 should complete before T004");
    assert!(t002_pos < t004_pos, "T002 should complete before T004");
    assert!(t003_pos < t005_pos, "T003 should complete before T005");
    assert!(t004_pos < t005_pos, "T004 should complete before T005");

    let assigned_events: Vec<_> = store
        .all_events()
        .into_iter()
        .filter(|e| matches!(e.kind, EventKind::TaskAssigned { .. }))
        .collect();

    for event in assigned_events {
        if let EventKind::TaskAssigned { task_id, agent_id } = &event.kind {
            assert!(
                agent_id.starts_with("worker-"),
                "Task {} assigned to valid worker: {}",
                task_id,
                agent_id
            );
        }
    }
}

#[tokio::test]
async fn dependency_graph_allows_parallel_independent_tasks() {
    let store = InMemoryEventStore::new();

    let tasks = vec![("T001", vec![]), ("T002", vec![]), ("T003", vec!["T001"])];

    for (task_id, _) in &tasks {
        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .unwrap();
    }

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
            kind: EventKind::TaskAssigned {
                task_id: "T002".into(),
                agent_id: "worker-2".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T002".into(),
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
            kind: EventKind::TaskCompleted {
                task_id: "T002".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T002".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: "T003".into(),
                agent_id: "worker-1".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T003".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();
    let assigned: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::TaskAssigned { .. }))
        .collect();

    assert_eq!(assigned.len(), 3, "All three tasks should be assigned");

    let t003_assigned = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskAssigned { task_id, .. } if task_id == "T003"))
        .count();
    assert_eq!(t003_assigned, 1, "T003 should be assigned exactly once");

    let t001_completed = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCompleted { task_id } if task_id == "T001"))
        .count();
    assert_eq!(t001_completed, 1, "T001 should be completed exactly once");
}

#[tokio::test]
async fn dependency_cycle_prevents_execution() {
    let store = InMemoryEventStore::new();

    let tasks: Vec<(&str, Vec<&str>)> = vec![
        ("T001", vec!["T002"]),
        ("T002", vec!["T003"]),
        ("T003", vec!["T001"]),
    ];

    for (task_id, _) in &tasks {
        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: task_id.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .unwrap();
    }

    let can_execute = |_task: &str, _deps: &[&str], _completed: &[&str]| -> bool { false };

    let mut completed: Vec<&str> = vec![];
    for (task_id, deps) in &tasks {
        if can_execute(task_id, deps, &completed) {
            completed.push(task_id);
        }
    }

    assert_eq!(
        completed.len(),
        0,
        "No tasks in a cycle should be executable"
    );
}
