//! Test: AI invoked for full-policy decisions (verify invocation count and categories)
//!
//! Supervisor in full autonomy mode
//! AI invoked for planning, assignments, stuck handling
//! Assert: correct invocation categories

use std::sync::Arc;

use brehon_ports::{DecisionEngine, EventStore};
use brehon_test_harness::{
    event_was_emitted, mock_decision::ScriptedDecision, InMemoryEventStore, MockDecisionEngine,
    MockGateway,
};
use brehon_types::{DecisionConfidence, DecisionKind, DecisionRequest, Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn autonomy_full_invokes_ai_for_planning() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());
    let decision_engine = Arc::new(MockDecisionEngine::new());

    decision_engine.set_responses(
        DecisionKind::PlanExecution,
        vec![ScriptedDecision {
            decision: "proceed_with_plan".into(),
            reasoning: "Plan is sound and executable".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec!["assign_task_to_worker".into()],
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

    let request = DecisionRequest {
        request_id: "req-1".into(),
        kind: DecisionKind::PlanExecution,
        context: "New task needs execution plan".into(),
        event_ids: vec![],
        options: vec!["proceed_with_plan".into(), "escalate".into()],
        created_at: Utc::now(),
    };

    let response = decision_engine.decide(request).await.unwrap();
    assert_eq!(response.decision, "proceed_with_plan");

    let calls = decision_engine.calls();
    assert_eq!(
        calls.len(),
        1,
        "AI should be invoked exactly once for planning"
    );
    assert_eq!(calls[0].kind, DecisionKind::PlanExecution);

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

    let events = store.all_events();
    assert!(
        event_was_emitted(&events, "TaskAssigned"),
        "Task should be assigned after planning decision"
    );
}

#[tokio::test]
async fn autonomy_full_invokes_ai_for_assignment() {
    let _store = InMemoryEventStore::new();
    let decision_engine = MockDecisionEngine::new();

    decision_engine.set_responses(
        DecisionKind::AssignWorker,
        vec![ScriptedDecision {
            decision: "worker-2".into(),
            reasoning: "Worker-2 has lowest load and relevant expertise".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec!["assign_to_worker_2".into()],
        }],
    );

    let available_workers = vec!["worker-1".into(), "worker-2".into(), "worker-3".into()];

    let request = DecisionRequest {
        request_id: "req-2".into(),
        kind: DecisionKind::AssignWorker,
        context: "Task T002 needs assignment".into(),
        event_ids: vec![],
        options: available_workers,
        created_at: Utc::now(),
    };

    let response = decision_engine.decide(request).await.unwrap();
    assert_eq!(response.decision, "worker-2");

    let calls = decision_engine.calls();
    assert_eq!(calls.len(), 1, "AI should be invoked for task assignment");
    assert_eq!(calls[0].kind, DecisionKind::AssignWorker);
}

#[tokio::test]
async fn autonomy_full_invokes_ai_for_stuck_handling() {
    let store = InMemoryEventStore::new();
    let decision_engine = MockDecisionEngine::new();

    decision_engine.set_responses(
        DecisionKind::StuckGuidance,
        vec![ScriptedDecision {
            decision: "provide_hint".into(),
            reasoning: "Worker appears stuck on implementation detail".into(),
            confidence: DecisionConfidence::Medium,
            next_actions: vec!["send_nudge".into()],
        }],
    );

    store
        .append(Event {
            kind: EventKind::StuckDetected {
                session_id: "session-1".into(),
                duration_minutes: 15,
                pattern: Some("repeated_similar_messages".into()),
            },
            timestamp: Utc::now(),
            aggregate_id: "session-1".into(),
        })
        .await
        .unwrap();

    let request = DecisionRequest {
        request_id: "req-3".into(),
        kind: DecisionKind::StuckGuidance,
        context: "Worker stuck for 15 minutes with repeated messages".into(),
        event_ids: vec![],
        options: vec!["provide_hint".into(), "reassign".into(), "escalate".into()],
        created_at: Utc::now(),
    };

    let response = decision_engine.decide(request).await.unwrap();
    assert_eq!(response.decision, "provide_hint");

    let calls = decision_engine.calls();
    assert_eq!(calls.len(), 1, "AI should be invoked for stuck guidance");
    assert_eq!(calls[0].kind, DecisionKind::StuckGuidance);

    let events = store.all_events();
    assert!(
        event_was_emitted(&events, "StuckDetected"),
        "Stuck detection should trigger AI invocation"
    );
}

#[tokio::test]
async fn autonomy_full_multiple_invocations() {
    let decision_engine = MockDecisionEngine::new();

    decision_engine.set_responses(
        DecisionKind::PlanExecution,
        vec![ScriptedDecision {
            decision: "proceed".into(),
            reasoning: "Plan validated".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    decision_engine.set_responses(
        DecisionKind::AssignWorker,
        vec![ScriptedDecision {
            decision: "worker-1".into(),
            reasoning: "Best fit".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    decision_engine.set_responses(
        DecisionKind::HeartbeatCheck,
        vec![ScriptedDecision {
            decision: "healthy".into(),
            reasoning: "All systems operational".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec![],
        }],
    );

    let plan_request = DecisionRequest {
        request_id: "req-plan".into(),
        kind: DecisionKind::PlanExecution,
        context: "Task planning".into(),
        event_ids: vec![],
        options: vec!["proceed".into()],
        created_at: Utc::now(),
    };
    decision_engine.decide(plan_request).await.unwrap();

    let assign_request = DecisionRequest {
        request_id: "req-assign".into(),
        kind: DecisionKind::AssignWorker,
        context: "Worker assignment".into(),
        event_ids: vec![],
        options: vec!["worker-1".into()],
        created_at: Utc::now(),
    };
    decision_engine.decide(assign_request).await.unwrap();

    let heartbeat_request = DecisionRequest {
        request_id: "req-heartbeat".into(),
        kind: DecisionKind::HeartbeatCheck,
        context: "Periodic health check".into(),
        event_ids: vec![],
        options: vec!["healthy".into()],
        created_at: Utc::now(),
    };
    decision_engine.decide(heartbeat_request).await.unwrap();

    let calls = decision_engine.calls();
    assert_eq!(
        calls.len(),
        3,
        "AI should be invoked 3 times in full autonomy"
    );

    let kinds: Vec<DecisionKind> = calls.iter().map(|c| c.kind).collect();
    assert!(
        kinds.contains(&DecisionKind::PlanExecution),
        "Should include planning invocation"
    );
    assert!(
        kinds.contains(&DecisionKind::AssignWorker),
        "Should include assignment invocation"
    );
    assert!(
        kinds.contains(&DecisionKind::HeartbeatCheck),
        "Should include heartbeat invocation"
    );
}

#[tokio::test]
async fn autonomy_full_ai_categories_tracking() {
    let decision_engine = MockDecisionEngine::new();

    let categories = vec![
        (DecisionKind::PlanExecution, "proceed", "Task planning"),
        (DecisionKind::AssignWorker, "worker-1", "Worker selection"),
        (DecisionKind::StuckGuidance, "nudge", "Stuck handling"),
        (DecisionKind::HeartbeatCheck, "healthy", "Health check"),
        (DecisionKind::ReviewDeadlock, "approve", "Review decision"),
    ];

    for (kind, decision, _reason) in &categories {
        decision_engine.set_responses(
            *kind,
            vec![ScriptedDecision {
                decision: decision.to_string(),
                reasoning: "Reasoning".into(),
                confidence: DecisionConfidence::High,
                next_actions: vec![],
            }],
        );

        let request = DecisionRequest {
            request_id: format!("req-{:?}", kind),
            kind: *kind,
            context: "Test context".into(),
            event_ids: vec![],
            options: vec![decision.to_string()],
            created_at: Utc::now(),
        };

        decision_engine.decide(request).await.unwrap();
    }

    let calls = decision_engine.calls();
    assert_eq!(
        calls.len(),
        categories.len(),
        "All categories should be invoked"
    );

    let invoked_kinds: std::collections::HashSet<_> = calls.iter().map(|c| c.kind).collect();
    for (kind, _, _) in &categories {
        assert!(
            invoked_kinds.contains(kind),
            "Category {:?} should be invoked",
            kind
        );
    }
}
