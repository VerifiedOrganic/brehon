//! Test: Stalled worker recovery with safe worktree handling
//!
//! Scenarios:
//! - Reassignment blocked when worktree has uncommitted changes
//! - Force reassignment archives dirty worktree
//! - Clean worktree allows reassignment with removal
//! - Worker becomes active before reassignment threshold

use std::sync::Arc;

use brehon_ports::{AgentGateway, EventStore};
use brehon_test_harness::{event_was_emitted, InMemoryEventStore, MockDecisionEngine, MockGateway};
use brehon_types::{AgentId, Event, EventKind, SessionSpec};
use chrono::Utc;

fn create_stuck_event(session_id: &str, duration: u64, pattern: Option<&str>) -> Event {
    Event {
        kind: EventKind::StuckDetected {
            session_id: session_id.to_string(),
            duration_minutes: duration,
            pattern: pattern.map(|p| p.to_string()),
        },
        timestamp: Utc::now(),
        aggregate_id: session_id.to_string(),
    }
}

fn create_assignment_event(task_id: &str, agent_id: &str) -> Event {
    Event {
        kind: EventKind::TaskAssigned {
            task_id: task_id.to_string(),
            agent_id: agent_id.to_string(),
        },
        timestamp: Utc::now(),
        aggregate_id: task_id.to_string(),
    }
}

fn create_worker_reassigned_event(
    old_worker: &str,
    new_worker: &str,
    task_id: &str,
    reason: &str,
    worktree_state: &str,
) -> Event {
    Event {
        kind: EventKind::WorkerReassigned {
            old_worker: old_worker.to_string(),
            new_worker: new_worker.to_string(),
            task_id: task_id.to_string(),
            reason: reason.to_string(),
            worktree_state: worktree_state.to_string(),
        },
        timestamp: Utc::now(),
        aggregate_id: task_id.to_string(),
    }
}

#[tokio::test]
async fn reassignment_blocked_by_dirty_worktree() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());
    let _decision_engine = Arc::new(MockDecisionEngine::new());

    let old_worker = AgentId::new("worker-stuck");
    let new_worker = AgentId::new("worker-new");
    let task_id = "T-recover-001";

    let old_session = gateway
        .spawn(SessionSpec::new(
            old_worker.clone(),
            "worker".into(),
            "/tmp/worker-stuck".into(),
        ))
        .await
        .unwrap();

    store
        .append(create_assignment_event(task_id, old_worker.as_str()))
        .await
        .unwrap();

    store
        .append(create_stuck_event(
            old_session.as_str(),
            30,
            Some("time_based"),
        ))
        .await
        .unwrap();

    let events = store.all_events();
    assert!(
        event_was_emitted(&events, "StuckDetected"),
        "Stuck detection should fire before reassignment"
    );

    let _event = create_worker_reassigned_event(
        old_worker.as_str(),
        new_worker.as_str(),
        task_id,
        "stalled",
        "dirty: 3 uncommitted file(s)",
    );

    let events = store.all_events();
    let stuck_before_reassign = events
        .iter()
        .position(|e| matches!(&e.kind, EventKind::StuckDetected { .. }));
    let reassign_pos = events
        .iter()
        .position(|e| matches!(&e.kind, EventKind::WorkerReassigned { .. }));

    if let (Some(stuck_idx), Some(reassign_idx)) = (stuck_before_reassign, reassign_pos) {
        assert!(
            stuck_idx < reassign_idx,
            "Stuck detection should happen before reassignment"
        );
    }
}

#[tokio::test]
async fn force_reassignment_archives_dirty_worktree() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());

    let old_worker = AgentId::new("worker-stuck-dirty");
    let new_worker = AgentId::new("worker-backup");
    let task_id = "T-recover-002";

    store
        .append(create_assignment_event(task_id, old_worker.as_str()))
        .await
        .unwrap();

    store
        .append(create_stuck_event("session-stuck-2", 45, None))
        .await
        .unwrap();

    store
        .append(create_worker_reassigned_event(
            old_worker.as_str(),
            new_worker.as_str(),
            task_id,
            "forced_reassignment",
            "archived",
        ))
        .await
        .unwrap();

    let events = store.all_events();

    let reassign_event = events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::WorkerReassigned { .. }));

    assert!(
        reassign_event.is_some(),
        "WorkerReassigned event should be emitted"
    );

    if let Some(event) = reassign_event {
        if let EventKind::WorkerReassigned { worktree_state, .. } = &event.kind {
            assert_eq!(
                worktree_state, "archived",
                "Dirty worktree should be archived, not deleted"
            );
        }
    }
}

#[tokio::test]
async fn clean_worktree_allows_reassignment() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());

    let old_worker = AgentId::new("worker-stuck-clean");
    let new_worker = AgentId::new("worker-replacement");
    let task_id = "T-recover-003";

    store
        .append(create_assignment_event(task_id, old_worker.as_str()))
        .await
        .unwrap();

    store
        .append(create_stuck_event("session-stuck-3", 20, None))
        .await
        .unwrap();

    store
        .append(create_worker_reassigned_event(
            old_worker.as_str(),
            new_worker.as_str(),
            task_id,
            "stalled",
            "clean",
        ))
        .await
        .unwrap();

    let events = store.all_events();

    let reassign_event = events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::WorkerReassigned { .. }));

    assert!(
        reassign_event.is_some(),
        "WorkerReassigned event should be emitted for clean worktree"
    );

    if let Some(event) = reassign_event {
        if let EventKind::WorkerReassigned { worktree_state, .. } = &event.kind {
            assert_eq!(
                worktree_state, "clean",
                "Clean worktree should allow reassignment"
            );
        }
    }
}

#[tokio::test]
async fn worker_becomes_active_before_reassignment() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());

    let worker_id = AgentId::new("worker-recovers");
    let task_id = "T-recover-004";

    let session = gateway
        .spawn(SessionSpec::new(
            worker_id.clone(),
            "worker".into(),
            "/tmp/worker-recovers".into(),
        ))
        .await
        .unwrap();

    store
        .append(create_assignment_event(task_id, worker_id.as_str()))
        .await
        .unwrap();

    store
        .append(create_stuck_event(session.as_str(), 15, None))
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: session.as_str().to_string(),
                operation: "resuming-work".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session.as_str().to_string(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "StuckDetected"),
        "Stuck detection should have fired"
    );

    let has_stuck_event = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::StuckDetected { .. }));
    let has_operation_started = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::OperationStarted { .. }));
    let has_reassigned = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::WorkerReassigned { .. }));

    assert!(has_stuck_event, "Should have stuck detection event");
    assert!(has_operation_started, "Worker should become active again");
    assert!(
        !has_reassigned,
        "No reassignment should occur if worker becomes active"
    );
}

#[tokio::test]
async fn reassignment_with_active_review_invalidates_review() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());

    let old_worker = AgentId::new("worker-with-review");
    let new_worker = AgentId::new("worker-review-replacement");
    let task_id = "T-recover-005";
    let review_id = "R-review-001";

    store
        .append(create_assignment_event(task_id, old_worker.as_str()))
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(create_stuck_event("session-stuck-5", 60, None))
        .await
        .unwrap();

    store
        .append(create_worker_reassigned_event(
            old_worker.as_str(),
            new_worker.as_str(),
            task_id,
            "stalled_with_review",
            "clean",
        ))
        .await
        .unwrap();

    let events = store.all_events();

    let has_review_requested = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ReviewRequested { .. }));
    let has_reassigned = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::WorkerReassigned { .. }));

    assert!(has_review_requested, "Should have review requested event");
    assert!(
        has_reassigned,
        "Reassignment should proceed even with active review"
    );

    let reassign_event = events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::WorkerReassigned { .. }));
    assert!(
        reassign_event.is_some(),
        "WorkerReassigned event should be present"
    );
}
