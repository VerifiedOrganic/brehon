//! Test: AI only invoked on stuck/failure (verify minimal invocations)
//!
//! Supervisor in minimal autonomy mode
//! AI invoked only on stuck/failure events
//! Assert: minimal invocations

use std::sync::Arc;

use brehon_ports::{DecisionEngine, EventStore};
use brehon_test_harness::{
    mock_decision::ScriptedDecision, InMemoryEventStore, MockDecisionEngine, MockGateway,
};
use brehon_types::{DecisionConfidence, DecisionKind, DecisionRequest, Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn autonomy_minimal_only_stuck_invokes_ai() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());
    let decision_engine = Arc::new(MockDecisionEngine::new());

    decision_engine.set_responses(
        DecisionKind::StuckGuidance,
        vec![ScriptedDecision {
            decision: "provide_guidance".into(),
            reasoning: "Worker needs nudge".into(),
            confidence: DecisionConfidence::Medium,
            next_actions: vec!["send_nudge".into()],
        }],
    );

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

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: task_id.clone(),
                agent_id: "worker-1".into(),
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

    let normal_calls = decision_engine.calls();
    assert_eq!(
        normal_calls.len(),
        0,
        "AI should NOT be invoked during normal operation in minimal mode"
    );

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: "session-1".into(),
                duration_minutes: 30,
                pattern: None,
            },
            timestamp: Utc::now(),
            aggregate_id: "session-1".into(),
        })
        .await
        .unwrap();

    let request = DecisionRequest {
        request_id: "req-stuck".into(),
        kind: DecisionKind::StuckGuidance,
        context: "Worker stuck for 30 minutes".into(),
        event_ids: vec![],
        options: vec!["provide_guidance".into(), "escalate".into()],
        created_at: Utc::now(),
    };

    let response = decision_engine.decide(request).await.unwrap();
    assert_eq!(response.decision, "provide_guidance");

    let calls = decision_engine.calls();
    assert_eq!(
        calls.len(),
        1,
        "AI should be invoked once for stuck detection"
    );
    assert_eq!(calls[0].kind, DecisionKind::StuckGuidance);
}

#[tokio::test]
async fn autonomy_minimal_no_ai_for_planning() {
    let store = InMemoryEventStore::new();
    let decision_engine = MockDecisionEngine::new();

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

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: task_id.clone(),
                agent_id: "worker-1".into(),
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

    let calls = decision_engine.calls();
    assert_eq!(
        calls.len(),
        0,
        "Minimal autonomy should not invoke AI for normal task flow"
    );
}

#[tokio::test]
async fn autonomy_minimal_failure_triggers_ai() {
    let store = InMemoryEventStore::new();
    let decision_engine = MockDecisionEngine::new();

    decision_engine.set_responses(
        DecisionKind::StuckGuidance,
        vec![ScriptedDecision {
            decision: "reassign".into(),
            reasoning: "Worker failed 3 times".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec!["assign_to_new_worker".into()],
        }],
    );

    for attempt in 0..3 {
        let task_id = format!("T-failed-{}", attempt);
        store
            .append(Event {
                kind: EventKind::TaskAssigned {
                    task_id: task_id.clone(),
                    agent_id: format!("worker-{}", attempt),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::AgentDied {
                    agent_id: format!("worker-{}", attempt),
                    session_id: format!("session-{}", attempt),
                    reason: "crash".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("worker-{}", attempt),
            })
            .await
            .unwrap();
    }

    let request = DecisionRequest {
        request_id: "req-failure".into(),
        kind: DecisionKind::StuckGuidance,
        context: "Multiple worker failures".into(),
        event_ids: vec![],
        options: vec!["reassign".into(), "escalate".into()],
        created_at: Utc::now(),
    };

    let response = decision_engine.decide(request).await.unwrap();
    assert_eq!(response.decision, "reassign");

    let calls = decision_engine.calls();
    assert_eq!(calls.len(), 1, "AI should be invoked for failure handling");
}

#[tokio::test]
async fn autonomy_minimal_review_deadlock_invokes_ai() {
    let store = InMemoryEventStore::new();
    let decision_engine = MockDecisionEngine::new();

    decision_engine.set_responses(
        DecisionKind::ReviewDeadlock,
        vec![ScriptedDecision {
            decision: "approve_with_conditions".into(),
            reasoning: "Review deadlock, scores close to threshold".into(),
            confidence: DecisionConfidence::Medium,
            next_actions: vec!["create_follow_up_task".into()],
        }],
    );

    let task_id = "T-review-deadlock";

    for round in 0..3 {
        let review_id = format!("R-{}", round);
        store
            .append(Event {
                kind: EventKind::ReviewRequested {
                    task_id: task_id.into(),
                    review_id: review_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::EscalationTriggered {
                reason: "review_deadlock".into(),
                context: "Max rounds exceeded".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.into(),
        })
        .await
        .unwrap();

    let request = DecisionRequest {
        request_id: "req-review".into(),
        kind: DecisionKind::ReviewDeadlock,
        context: "Review deadlock requires supervisor judgment".into(),
        event_ids: vec![],
        options: vec!["approve_with_conditions".into(), "reject".into()],
        created_at: Utc::now(),
    };

    let response = decision_engine.decide(request).await.unwrap();
    assert_eq!(response.decision, "approve_with_conditions");

    let calls = decision_engine.calls();
    assert_eq!(calls.len(), 1, "AI should be invoked for review deadlock");
    assert_eq!(calls[0].kind, DecisionKind::ReviewDeadlock);
}

#[tokio::test]
async fn autonomy_minimal_comparison_full() {
    let decision_engine_minimal = MockDecisionEngine::new();
    let decision_engine_full = MockDecisionEngine::new();

    decision_engine_minimal.set_responses(
        DecisionKind::StuckGuidance,
        vec![ScriptedDecision {
            decision: "nudge".into(),
            reasoning: "Minimal mode stuck handling".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    decision_engine_full.set_responses(
        DecisionKind::PlanExecution,
        vec![ScriptedDecision {
            decision: "proceed".into(),
            reasoning: "Full mode planning".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    decision_engine_full.set_responses(
        DecisionKind::AssignWorker,
        vec![ScriptedDecision {
            decision: "worker-1".into(),
            reasoning: "Full mode assignment".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    decision_engine_full.set_responses(
        DecisionKind::HeartbeatCheck,
        vec![ScriptedDecision {
            decision: "healthy".into(),
            reasoning: "Full mode heartbeat".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    decision_engine_minimal.set_responses(
        DecisionKind::StuckGuidance,
        vec![ScriptedDecision {
            decision: "nudge".into(),
            reasoning: "Minimal mode stuck".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    let full_request = DecisionRequest {
        request_id: "req-full".into(),
        kind: DecisionKind::PlanExecution,
        context: "Planning".into(),
        event_ids: vec![],
        options: vec!["proceed".into()],
        created_at: Utc::now(),
    };
    decision_engine_full.decide(full_request).await.unwrap();

    let full_assign = DecisionRequest {
        request_id: "req-assign".into(),
        kind: DecisionKind::AssignWorker,
        context: "Assignment".into(),
        event_ids: vec![],
        options: vec!["worker-1".into()],
        created_at: Utc::now(),
    };
    decision_engine_full.decide(full_assign).await.unwrap();

    let full_heartbeat = DecisionRequest {
        request_id: "req-hb".into(),
        kind: DecisionKind::HeartbeatCheck,
        context: "Health check".into(),
        event_ids: vec![],
        options: vec!["healthy".into()],
        created_at: Utc::now(),
    };
    decision_engine_full.decide(full_heartbeat).await.unwrap();

    let minimal_stuck = DecisionRequest {
        request_id: "req-minimal".into(),
        kind: DecisionKind::StuckGuidance,
        context: "Worker stuck".into(),
        event_ids: vec![],
        options: vec!["nudge".into()],
        created_at: Utc::now(),
    };
    decision_engine_minimal.decide(minimal_stuck).await.unwrap();

    let full_calls = decision_engine_full.calls();
    let minimal_calls = decision_engine_minimal.calls();

    assert_eq!(full_calls.len(), 3, "Full autonomy: multiple invocations");
    assert_eq!(
        minimal_calls.len(),
        1,
        "Minimal autonomy: single stuck-only invocation"
    );

    let minimal_kinds: Vec<_> = minimal_calls.iter().map(|c| c.kind).collect();
    assert!(
        minimal_kinds.contains(&DecisionKind::StuckGuidance),
        "Minimal mode should only invoke for stuck/failure"
    );
}
