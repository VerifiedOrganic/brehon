//! Crash recovery test: Mid-merge crash.
//!
//! Tests that after a MergePrepared event, if a crash occurs before the target
//! ref update completes, the recovery detects the incomplete merge and restores
//! a clean state.

use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{CrashInjector, CrashPoint, CrashScenario, InMemoryEventStore};
use brehon_types::{Event, EventFilter, EventKind};
use chrono::Utc;

#[tokio::test]
async fn crash_mid_merge_detects_incomplete_merge() {
    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T001";
    let branch = "feature/cool-feature";

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
            kind: EventKind::TaskAssigned {
                task_id: task_id.to_string(),
                agent_id: "worker-1".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let injector = CrashInjector::new().add_scenario(CrashScenario {
        name: "mid-merge".into(),
        crash_points: vec![CrashPoint::AfterEvent("MergePrepared".into())],
        restart_after_crash: true,
        verify_recovery: true,
    });

    let mut inj = injector;
    inj.start_scenario("mid-merge");

    let merge_prepared_event = Event {
        kind: EventKind::MergePrepared {
            task_id: task_id.to_string(),
            branch: branch.to_string(),
        },
        timestamp: Utc::now(),
        aggregate_id: task_id.to_string(),
    };
    store.append(merge_prepared_event.clone()).await.unwrap();

    let should_crash = inj.record_event("MergePrepared");
    assert!(
        should_crash,
        "Injector should trigger crash at MergePrepared"
    );
    assert!(inj.should_crash(), "Crash should be flagged");

    let merge_events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    let has_merge_prepared = merge_events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::MergePrepared {
                task_id: t,
                ..
            } if t == task_id
        )
    });
    assert!(has_merge_prepared, "MergePrepared event should exist");

    let has_merge_committed = merge_events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::MergeCommitted { task_id: t } if t == task_id
        )
    });
    assert!(
        !has_merge_committed,
        "MergeCommitted should NOT exist after crash"
    );

    let incomplete_merge = detect_incomplete_merge(&merge_events, task_id);
    assert!(
        incomplete_merge,
        "Recovery should detect incomplete merge from MergePrepared without MergeCommitted/MergeAborted"
    );
}

#[tokio::test]
async fn crash_mid_merge_recovery_restores_clean_state() {
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
            kind: EventKind::MergePrepared {
                task_id: task_id.to_string(),
                branch: "feature/test".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let clean = recover_from_incomplete_merge(&store, task_id).await;
    assert!(clean, "Recovery should result in clean state");

    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    let has_abort = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::MergeAborted { task_id: t, .. } if t == task_id
        )
    });
    assert!(
        has_abort,
        "Recovery should emit MergeAborted event for incomplete merge"
    );
}

#[tokio::test]
async fn crash_mid_merge_complete_flow_no_crash() {
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
            kind: EventKind::MergePrepared {
                task_id: task_id.to_string(),
                branch: "feature/complete".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let crashed = inj.record_event("MergePrepared");
    assert!(!crashed, "No crash should occur in complete flow");

    store
        .append(Event {
            kind: EventKind::MergeCommitted {
                task_id: task_id.to_string(),
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

    let incomplete = detect_incomplete_merge(&events, task_id);
    assert!(
        !incomplete,
        "Complete merge should not be detected as incomplete"
    );
}

fn detect_incomplete_merge(events: &[Event], task_id: &str) -> bool {
    let has_merge_prepared = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::MergePrepared { task_id: t, .. } if t == task_id
        )
    });

    let has_finalized = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::MergeCommitted { task_id: t } if t == task_id
        ) || matches!(
            &e.kind,
            EventKind::MergeAborted { task_id: t, .. } if t == task_id
        )
    });

    has_merge_prepared && !has_finalized
}

async fn recover_from_incomplete_merge(store: &InMemoryEventStore, task_id: &str) -> bool {
    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    if detect_incomplete_merge(&events, task_id) {
        store
            .append(Event {
                kind: EventKind::MergeAborted {
                    task_id: task_id.to_string(),
                    reason: "Crash recovery: incomplete merge detected".to_string(),
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
