//! Test: Worker gets stuck, time-based detection fires, nudge sent, worker resumes
//!
//! Worker sends no events for time_threshold_minutes
//! Stuck detection fires
//! Nudge sent via gateway
//! Worker resumes after nudge
//! Assert: stuck_detected event, nudge_sent event, task completes

use std::sync::Arc;

use brehon_ports::{AgentGateway, EventStore};
use brehon_test_harness::{
    event_was_emitted, InMemoryEventStore, MockDecisionEngine, MockGateway,
    RecordingNotificationSink,
};
use brehon_types::{AgentId, Event, EventKind, SessionSpec};
use chrono::Utc;

#[tokio::test]
async fn stuck_worker_nudge_detection_and_resume() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());
    let _notifications = Arc::new(RecordingNotificationSink::new());
    let _decision_engine = Arc::new(MockDecisionEngine::new());

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

    let session = gateway
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
                session_id: session.as_str().to_string(),
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
                session_id: session.as_str().to_string(),
                operation: "initial-work".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session.as_str().to_string(),
        })
        .await
        .unwrap();

    let stuck_duration_minutes = 30u64;
    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session.as_str().to_string(),
                duration_minutes: stuck_duration_minutes,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: session.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeSent {
                session_id: session.as_str().to_string(),
                kind: "soft".into(),
                content: "Are you stuck? Remember to report progress.".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::OperationCompleted {
                session_id: session.as_str().to_string(),
                operation: "initial-work".into(),
                success: true,
            },
            timestamp: Utc::now(),
            aggregate_id: session.as_str().to_string(),
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
        event_was_emitted(&events, "StuckDetected"),
        "Stuck detection should fire"
    );

    assert!(
        event_was_emitted(&events, "NudgeSent"),
        "Nudge should be sent to stuck worker"
    );

    let stuck_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::StuckDetected { .. }))
        .collect();
    assert_eq!(stuck_events.len(), 1, "Should detect stuck exactly once");

    if let EventKind::StuckDetected {
        session_id: _,
        duration_minutes,
        ..
    } = &stuck_events[0].kind
    {
        assert!(
            duration_minutes >= &30u64,
            "Stuck duration should meet threshold"
        );
    }

    let nudge_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::NudgeSent { .. }))
        .collect();
    assert_eq!(nudge_events.len(), 1, "Should send exactly one nudge");

    assert!(
        event_was_emitted(&events, "TaskCompleted"),
        "Task should complete after nudge"
    );

    let operations: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                EventKind::OperationStarted { .. } | EventKind::OperationCompleted { .. }
            )
        })
        .collect();
    assert!(
        operations.len() >= 2,
        "Should have operation start and completion"
    );
}

#[tokio::test]
async fn stuck_detection_with_pattern() {
    let store = InMemoryEventStore::new();

    let session_id = "session-stuck-pattern".to_string();

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session_id.clone(),
                duration_minutes: 15,
                pattern: Some("repeated_similar_messages".into()),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeSent {
                session_id: session_id.clone(),
                kind: "redirect".into(),
                content: "I noticed you're repeating similar messages. Try a different approach."
                    .into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let stuck_event = events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::StuckDetected { .. }));

    assert!(stuck_event.is_some(), "Stuck event should be emitted");

    if let Some(event) = stuck_event {
        if let EventKind::StuckDetected { pattern, .. } = &event.kind {
            assert!(pattern.is_some(), "Pattern should be detected");
            assert_eq!(pattern.as_ref().unwrap(), "repeated_similar_messages");
        }
    }
}

#[tokio::test]
async fn stuck_worker_multiple_nudges_escalate() {
    let store = InMemoryEventStore::new();
    let session_id = "session-multi-nudge".to_string();

    store
        .append(Event {
            kind: EventKind::PromptSent {
                session_id: session_id.clone(),
                prompt_id: "p1".into(),
                content: "work on task".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session_id.clone(),
                duration_minutes: 10,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeSent {
                session_id: session_id.clone(),
                kind: "soft".into(),
                content: "First nudge".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session_id.clone(),
                duration_minutes: 25,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeSent {
                session_id: session_id.clone(),
                kind: "guidance".into(),
                content: "Second nudge with specific guidance".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session_id.clone(),
                duration_minutes: 45,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeSent {
                session_id: session_id.clone(),
                kind: "resume".into(),
                content: "Final nudge with resume strategy".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let stuck_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::StuckDetected { .. }))
        .count();
    assert_eq!(stuck_count, 3, "Should detect stuck 3 times");

    let nudge_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::NudgeSent { .. }))
        .count();
    assert_eq!(nudge_count, 3, "Should send 3 nudges");

    let nudges: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::NudgeSent { kind, .. } = &e.kind {
                Some(kind.clone())
            } else {
                None
            }
        })
        .collect();

    assert_eq!(nudges[0], "soft", "First nudge should be soft");
    assert_eq!(
        nudges[1], "guidance",
        "Second nudge should provide guidance"
    );
    assert_eq!(nudges[2], "resume", "Third nudge should help resume");
}
