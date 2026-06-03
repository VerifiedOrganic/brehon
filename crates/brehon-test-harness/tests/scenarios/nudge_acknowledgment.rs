//! Test: Nudge acknowledgment and acted-on lifecycle
//!
//! Worker receives nudge, acknowledges it, and makes progress
//! Assert: nudge state transitions: Delivered → Acknowledged → ActedOn

use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{event_was_emitted, InMemoryEventStore};
use brehon_types::{AgentId, Event, EventKind, NudgeDeliveryState};
use chrono::Utc;

#[tokio::test]
async fn nudge_lifecycle_transitions_to_acknowledged_on_response() {
    let store = Arc::new(InMemoryEventStore::new());

    let worker_id = AgentId::new("worker-1");
    let session_id = "session-nudge-1";

    store
        .append(Event {
            kind: EventKind::AgentSpawned {
                agent_id: worker_id.as_str().to_string(),
                session_id: session_id.to_string(),
                role: "worker".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: worker_id.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session_id.to_string(),
                duration_minutes: 15,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeSent {
                session_id: session_id.to_string(),
                kind: "soft".into(),
                content: "Are you still working on this?".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    let events = store.all_events();
    assert!(
        event_was_emitted(&events, "NudgeSent"),
        "Nudge should be sent"
    );

    store
        .append(Event {
            kind: EventKind::ResponseReceived {
                session_id: session_id.to_string(),
                prompt_id: "response-1".into(),
                tokens_used: 50,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeAcknowledged {
                session_id: session_id.to_string(),
                nudge_kind: "soft".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    let events = store.all_events();
    assert!(
        event_was_emitted(&events, "NudgeAcknowledged"),
        "Nudge should be acknowledged after response"
    );

    let ack_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::NudgeAcknowledged { .. }))
        .collect();
    assert_eq!(
        ack_events.len(),
        1,
        "Should have exactly one acknowledgment"
    );
}

#[tokio::test]
async fn nudge_lifecycle_transitions_to_acted_on_after_progress() {
    let store = Arc::new(InMemoryEventStore::new());

    let worker_id = AgentId::new("worker-2");
    let session_id = "session-nudge-2";

    store
        .append(Event {
            kind: EventKind::AgentSpawned {
                agent_id: worker_id.as_str().to_string(),
                session_id: session_id.to_string(),
                role: "worker".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: worker_id.as_str().to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session_id.to_string(),
                duration_minutes: 20,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeSent {
                session_id: session_id.to_string(),
                kind: "guidance".into(),
                content: "Try a different approach".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ResponseReceived {
                session_id: session_id.to_string(),
                prompt_id: "response-2".into(),
                tokens_used: 100,
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeAcknowledged {
                session_id: session_id.to_string(),
                nudge_kind: "guidance".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::NudgeActedOn {
                session_id: session_id.to_string(),
                nudge_kind: "guidance".into(),
                progress_type: "commit".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.to_string(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "NudgeSent"),
        "Nudge should be sent"
    );
    assert!(
        event_was_emitted(&events, "NudgeAcknowledged"),
        "Nudge should be acknowledged"
    );
    assert!(
        event_was_emitted(&events, "NudgeActedOn"),
        "Nudge should be acted on after progress"
    );

    let acted_on_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::NudgeActedOn { .. }))
        .collect();
    assert_eq!(acted_on_events.len(), 1);

    if let EventKind::NudgeActedOn { progress_type, .. } = &acted_on_events[0].kind {
        assert_eq!(progress_type, "commit");
    }
}

#[tokio::test]
async fn nudge_remains_delivered_if_worker_stays_silent() {
    let store = InMemoryEventStore::new();

    let session_id = "session-silent".to_string();

    store
        .append(Event {
            kind: EventKind::AgentSpawned {
                agent_id: "worker-silent".into(),
                session_id: session_id.clone(),
                role: "worker".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "worker-silent".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: session_id.clone(),
                duration_minutes: 30,
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
                content: "Are you still there?".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: session_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "NudgeSent"),
        "Nudge should be sent"
    );

    assert!(
        !event_was_emitted(&events, "NudgeAcknowledged"),
        "Nudge should not be acknowledged if worker silent"
    );

    assert!(
        !event_was_emitted(&events, "NudgeActedOn"),
        "Nudge should not be acted on if worker silent"
    );

    let nudge_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::NudgeSent { .. }))
        .collect();
    assert_eq!(nudge_events.len(), 1);
}

#[tokio::test]
async fn nudge_delivery_state_enum_values() {
    assert_eq!(NudgeDeliveryState::Delivered.to_string(), "delivered");
    assert_eq!(NudgeDeliveryState::Acknowledged.to_string(), "acknowledged");
    assert_eq!(NudgeDeliveryState::ActedOn.to_string(), "acted_on");
    assert_eq!(NudgeDeliveryState::TimedOut.to_string(), "timed_out");

    assert_ne!(
        NudgeDeliveryState::Delivered,
        NudgeDeliveryState::Acknowledged
    );
    assert_ne!(
        NudgeDeliveryState::Acknowledged,
        NudgeDeliveryState::ActedOn
    );
    assert_ne!(NudgeDeliveryState::Delivered, NudgeDeliveryState::ActedOn);
    assert_ne!(NudgeDeliveryState::Delivered, NudgeDeliveryState::TimedOut);
}
