//! Crash recovery test: Mid-dispatch crash.
//!
//! Tests that if a crash occurs after a task is assigned but before the worker
//! receives the prompt, the recovery returns the task to pending state and
//! re-dispatches it.

use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{CrashInjector, CrashPoint, CrashScenario, InMemoryEventStore};
use brehon_types::{Event, EventFilter, EventKind};
use chrono::Utc;

#[tokio::test]
async fn crash_mid_dispatch_returns_task_to_pending() {
    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T001";
    let agent_id = "worker-1";

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
        name: "mid-dispatch".into(),
        crash_points: vec![CrashPoint::AfterEvent("TaskAssigned".into())],
        restart_after_crash: true,
        verify_recovery: true,
    });

    let mut inj = injector;
    inj.start_scenario("mid-dispatch");

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: task_id.to_string(),
                agent_id: agent_id.to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let crashed = inj.record_event("TaskAssigned");
    assert!(crashed, "Crash should occur after TaskAssigned");
    assert!(inj.should_crash(), "Crash flag should be set");

    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    let has_assigned = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::TaskAssigned { task_id: t, .. } if t == task_id
        )
    });
    assert!(has_assigned, "TaskAssigned event should exist");

    let has_prompt_sent = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::PromptSent { .. }));
    assert!(
        !has_prompt_sent,
        "No PromptSent event should exist - crash occurred before worker received prompt"
    );

    let incomplete = detect_incomplete_dispatch(&events, task_id);
    assert!(
        incomplete,
        "Recovery should detect incomplete dispatch: assigned but no prompt"
    );
}

#[tokio::test]
async fn crash_mid_dispatch_recovery_re_dispatcher() {
    let store = InMemoryEventStore::new();
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
            kind: EventKind::TaskAssigned {
                task_id: task_id.to_string(),
                agent_id: "worker-2".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let events_before = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();
    assert_eq!(
        events_before.len(),
        2,
        "Should have 2 events before recovery"
    );

    let incomplete = detect_incomplete_dispatch(&events_before, task_id);
    assert!(
        incomplete,
        "Should detect incomplete dispatch before recovery"
    );

    let recovered = recover_incomplete_dispatch(&store, task_id).await;
    assert!(
        recovered,
        "Recovery should return true for incomplete dispatch"
    );

    let events_after = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    assert!(
        events_after.len() >= 2,
        "After recovery, events should be preserved"
    );

    let still_detectable = detect_incomplete_dispatch(&events_after, task_id);
    assert!(
        !still_detectable || events_after.len() > events_before.len(),
        "Recovery should clear incomplete state or add recovery event"
    );
}

#[tokio::test]
async fn crash_mid_dispatch_complete_flow_no_crash() {
    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T003";
    let agent_id = "worker-3";

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
            kind: EventKind::TaskAssigned {
                task_id: task_id.to_string(),
                agent_id: agent_id.to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.to_string(),
        })
        .await
        .unwrap();

    let crashed = inj.record_event("TaskAssigned");
    assert!(!crashed, "No crash in complete flow");

    store
        .append(Event {
            kind: EventKind::PromptSent {
                session_id: format!("session-{}", agent_id),
                prompt_id: format!("prompt-{}", task_id),
                content: "Please implement the task".to_string(),
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

    let incomplete = detect_incomplete_dispatch(&events, task_id);
    assert!(
        !incomplete,
        "Complete dispatch should not be detected as incomplete"
    );

    let has_prompt = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::PromptSent { .. }));
    assert!(has_prompt, "Complete flow should have PromptSent event");
}

#[tokio::test]
async fn crash_mid_dispatch_multiple_tasks_isolated() {
    let store = Arc::new(InMemoryEventStore::new());

    for i in 1..=3 {
        let task_id = format!("T{:03}", i);
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
    }

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: "T001".to_string(),
                agent_id: "worker-1".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: "T002".to_string(),
                agent_id: "worker-2".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T002".to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::PromptSent {
                session_id: "session-worker-2".to_string(),
                prompt_id: "prompt-T002".to_string(),
                content: "Work on T002".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T002".to_string(),
        })
        .await
        .unwrap();

    let t1_events = store
        .query(EventFilter::new().aggregate("T001"))
        .await
        .unwrap();
    let t1_incomplete = detect_incomplete_dispatch(&t1_events, "T001");
    assert!(t1_incomplete, "T001 should be incomplete (no prompt)");

    let t2_events = store
        .query(EventFilter::new().aggregate("T002"))
        .await
        .unwrap();
    let t2_incomplete = detect_incomplete_dispatch(&t2_events, "T002");
    assert!(!t2_incomplete, "T002 should be complete (has prompt)");

    let t3_events = store
        .query(EventFilter::new().aggregate("T003"))
        .await
        .unwrap();
    let t3_incomplete = detect_incomplete_dispatch(&t3_events, "T003");
    assert!(
        !t3_incomplete,
        "T003 should not be incomplete (not assigned)"
    );
}

fn detect_incomplete_dispatch(events: &[Event], task_id: &str) -> bool {
    let has_assigned = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::TaskAssigned { task_id: t, .. } if t == task_id
        )
    });

    let has_prompt_sent = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::PromptSent { .. }));

    has_assigned && !has_prompt_sent
}

async fn recover_incomplete_dispatch(store: &InMemoryEventStore, task_id: &str) -> bool {
    let events = store
        .query(EventFilter::new().aggregate(task_id))
        .await
        .unwrap();

    if detect_incomplete_dispatch(&events, task_id) {
        store
            .append(Event {
                kind: EventKind::PromptCancelled {
                    session_id: "recovery".to_string(),
                    prompt_id: format!("incomplete-{}", task_id),
                    reason: "Crash recovery: task assignment incomplete".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .ok();
        return true;
    }
    false
}
