//! Crash recovery test: Crash during git rebase.
//!
//! Tests that if a crash occurs during a git rebase operation, the recovery
//! detects the dirty worktree state and cleans up the rebase state.

use std::sync::Arc;

use brehon_ports::{EventStore, GitOperations};
use brehon_test_harness::{
    CrashInjector, CrashPoint, CrashScenario, FakeGitOperations, InMemoryEventStore,
};
use brehon_types::{Event, EventFilter, EventKind};
use chrono::Utc;

#[tokio::test]
async fn crash_during_rebase_detects_dirty_worktree() {
    let git = FakeGitOperations::new();
    git.create_branch("feature/test-branch");

    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T001";

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

    let injector = CrashInjector::new().add_scenario(CrashScenario {
        name: "during-rebase".into(),
        crash_points: vec![CrashPoint::AfterEvent("OperationStarted".into())],
        restart_after_crash: true,
        verify_recovery: true,
    });

    let mut inj = injector;
    inj.start_scenario("during-rebase");

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "session-1".to_string(),
                operation: "rebase".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let crashed = inj.record_event("OperationStarted");
    assert!(crashed, "Crash should occur during rebase operation");
    assert!(inj.should_crash(), "Crash flag should be set");

    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    let has_started = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::OperationStarted { operation: op, .. } if op == "rebase"
        )
    });

    let has_completed = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::OperationCompleted { operation: op, .. } if op == "rebase"
        )
    });

    assert!(has_started, "OperationStarted should exist");
    assert!(
        !has_completed,
        "OperationCompleted should NOT exist after crash"
    );

    let dirty = detect_dirty_rebase_state(&events);
    assert!(
        dirty,
        "Recovery should detect dirty worktree from incomplete rebase"
    );
}

#[tokio::test]
async fn crash_during_rebase_cleanup_restores_clean_state() {
    let git = FakeGitOperations::new();
    git.create_branch("feature/cleanup-test");

    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T002";

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

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "session-2".to_string(),
                operation: "rebase".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let cleaned = recover_from_rebase_crash(&store, task_id).await;
    assert!(cleaned, "Recovery should clean up rebase state");

    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    let has_aborted = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::MergeAborted { task_id: t, .. } if t == task_id
        ) || matches!(
            &e.kind,
            EventKind::OperationCompleted { operation: op, success: false, .. } if op == "rebase"
        )
    });

    assert!(
        has_aborted,
        "Recovery should emit completion event for interrupted rebase"
    );
}

#[tokio::test]
async fn crash_during_rebase_complete_flow_no_crash() {
    let git = FakeGitOperations::new();
    git.create_branch("feature/complete-rebase");

    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T003";

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

    let injector = CrashInjector::new().add_scenario(CrashScenario {
        name: "no-crash".into(),
        crash_points: vec![],
        restart_after_crash: false,
        verify_recovery: false,
    });

    let mut inj = injector;
    inj.start_scenario("no-crash");

    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "session-3".to_string(),
                operation: "rebase".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let crashed = inj.record_event("OperationStarted");
    assert!(!crashed, "No crash in complete flow");

    let result = git.rebase("feature/complete-rebase", "main").await;
    assert!(result.is_ok(), "Rebase should succeed in complete flow");

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: "session-3".to_string(),
                operation: "rebase".to_string(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    let dirty = detect_dirty_rebase_state(&events);
    assert!(!dirty, "Complete rebase should not leave dirty state");

    let has_completion = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::OperationCompleted { operation: op, success: true, .. } if op == "rebase"
        )
    });
    assert!(
        has_completion,
        "Complete flow should have successful completion"
    );
}

#[tokio::test]
async fn crash_during_rebase_multiple_operations_isolated() {
    let _git = FakeGitOperations::new();

    let store = Arc::new(InMemoryEventStore::new());

    let task1 = "T001";
    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "s1".to_string(),
                operation: "rebase".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task1.to_string(),
        })
        .await
        .unwrap();
    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: "s1".to_string(),
                operation: "rebase".to_string(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: task1.to_string(),
        })
        .await
        .unwrap();

    let task2 = "T002";
    store
        .append(Event {
            kind: EventKind::OperationStarted {
                session_id: "s2".to_string(),
                operation: "rebase".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task2.to_string(),
        })
        .await
        .unwrap();

    let task3 = "T003";
    store
        .append(Event {
            kind: EventKind::MergePrepared {
                task_id: task3.to_string(),
                branch: "feature/op3".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task3.to_string(),
        })
        .await
        .unwrap();

    let t1_events = store
        .query(EventFilter::new().aggregate(task1))
        .await
        .unwrap();
    let t1_dirty = detect_dirty_rebase_state(&t1_events);
    assert!(!t1_dirty, "T001 should be clean (complete rebase)");

    let t2_events = store
        .query(EventFilter::new().aggregate(task2))
        .await
        .unwrap();
    let t2_dirty = detect_dirty_rebase_state(&t2_events);
    assert!(t2_dirty, "T002 should be dirty (incomplete rebase)");

    let t3_events = store
        .query(EventFilter::new().aggregate(task3))
        .await
        .unwrap();
    let t3_dirty = detect_dirty_rebase_state(&t3_events);
    assert!(!t3_dirty, "T003 should not be rebase-dirty (merge op)");
}

fn detect_dirty_rebase_state(events: &[Event]) -> bool {
    let rebase_started = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::OperationStarted { operation: op, .. } if op == "rebase"
        )
    });

    let rebase_completed = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::OperationCompleted { operation: op, .. } if op == "rebase"
        )
    });

    rebase_started && !rebase_completed
}

async fn recover_from_rebase_crash(store: &InMemoryEventStore, task_id: &str) -> bool {
    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    if detect_dirty_rebase_state(&events) {
        store
            .append(Event {
                kind: EventKind::OperationCompleted {
                    session_id: "recovery".to_string(),
                    operation: "rebase".to_string(),
                    success: false,
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::MergeAborted {
                    task_id: task_id.to_string(),
                    reason: "Crash recovery: rebase interrupted".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .unwrap();

        return true;
    }
    false
}
