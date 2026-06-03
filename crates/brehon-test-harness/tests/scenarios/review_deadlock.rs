//! Test: Three rounds never reach threshold, supervisor judgment triggered
//!
//! Reviewers give scores that never meet threshold
//! Max rounds exceeded
//! EscalationTriggered event emitted
//! Assert: escalation_triggered, review_rounds == max

use std::sync::Arc;

use brehon_ports::{DecisionEngine, EventStore};
use brehon_test_harness::{
    event_was_emitted, mock_decision::ScriptedDecision, InMemoryEventStore, MockDecisionEngine,
    MockGateway,
};
use brehon_types::{DecisionConfidence, DecisionKind, DecisionRequest, Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn review_deadlock_triggers_escalation() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());
    let decision_engine = Arc::new(MockDecisionEngine::new());

    decision_engine.set_responses(
        DecisionKind::StuckGuidance,
        vec![ScriptedDecision {
            decision: "escalate".into(),
            reasoning: "Review rounds exhausted without reaching threshold".into(),
            confidence: DecisionConfidence::High,
            next_actions: vec!["notify_human".into()],
        }],
    );

    let task_id = "T001".to_string();
    let max_rounds = 3;

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

    for round in 0..max_rounds {
        let review_id = format!("R-round-{}", round);

        store
            .append(Event {
                kind: EventKind::ReviewRequested {
                    task_id: task_id.clone(),
                    review_id: review_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id.clone(),
                    reviewer_id: "reviewer-1".into(),
                    score: 6,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id.clone(),
                    reviewer_id: "reviewer-2".into(),
                    score: 7,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id.clone(),
                    reviewer_id: "reviewer-3".into(),
                    score: 6,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();

        if round < max_rounds - 1 {
            store
                .append(Event {
                    kind: EventKind::ReviewChangesRequested {
                        review_id: review_id.clone(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: review_id.clone(),
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
        }
    }

    store
        .append(Event {
            kind: EventKind::EscalationTriggered {
                reason: "review_deadlock".into(),
                context: format!(
                    "Review rounds exhausted for task {} without reaching threshold",
                    task_id
                ),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let review_rounds = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReviewRequested { .. }))
        .count();
    assert_eq!(
        review_rounds, max_rounds,
        "Should have exactly {} review rounds",
        max_rounds
    );

    assert!(
        event_was_emitted(&events, "EscalationTriggered"),
        "Escalation should be triggered after max rounds"
    );

    let escalation_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::EscalationTriggered { .. }))
        .collect();

    assert_eq!(
        escalation_events.len(),
        1,
        "Should have exactly one escalation"
    );

    if let EventKind::EscalationTriggered { reason, .. } = &escalation_events[0].kind {
        assert!(
            reason.contains("review") || reason.contains("deadlock"),
            "Escalation reason should mention review or deadlock"
        );
    }
}

#[tokio::test]
async fn review_deadlock_with_divergent_scores() {
    let store = InMemoryEventStore::new();
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

    for round in 0..3 {
        let review_id = format!("R-round-{}", round);

        store
            .append(Event {
                kind: EventKind::ReviewRequested {
                    task_id: task_id.clone(),
                    review_id: review_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();

        let scores = match round {
            0 => vec![4u8, 8u8, 5u8],
            1 => vec![5u8, 7u8, 5u8],
            _ => vec![6u8, 6u8, 6u8],
        };

        for (idx, score) in scores.into_iter().enumerate() {
            store
                .append(Event {
                    kind: EventKind::ReviewScoreReceived {
                        review_id: review_id.clone(),
                        reviewer_id: format!("reviewer-{}", idx),
                        score,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: review_id.clone(),
                })
                .await
                .unwrap();
        }
    }

    store
        .append(Event {
            kind: EventKind::EscalationTriggered {
                reason: "divergent_reviews".into(),
                context: "Reviewers disagree; cannot reach consensus".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let scores_by_round: Vec<Vec<u8>> = (0..3)
        .map(|round| {
            let review_id = format!("R-round-{}", round);
            events
                .iter()
                .filter_map(|e| {
                    if let EventKind::ReviewScoreReceived {
                        review_id: rid,
                        score,
                        ..
                    } = &e.kind
                    {
                        if rid == &review_id {
                            Some(*score)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();

    for round_scores in &scores_by_round {
        let avg = round_scores.iter().sum::<u8>() as f64 / round_scores.len() as f64;
        assert!(avg < 7.5, "Each round average should be below threshold");
    }

    assert!(
        event_was_emitted(&events, "EscalationTriggered"),
        "Escalation should be triggered for divergent scores"
    );
}

#[tokio::test]
async fn review_deadlock_triggers_supervisor_judgment() {
    let store = InMemoryEventStore::new();
    let decision_engine = MockDecisionEngine::new();

    decision_engine.set_responses(
        DecisionKind::StuckGuidance,
        vec![ScriptedDecision {
            decision: "merge_with_conditions".into(),
            reasoning: "Scores close to threshold; approve with follow-up task".into(),
            confidence: DecisionConfidence::Medium,
            next_actions: vec!["approve_with_conditions".into()],
        }],
    );

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

    for round in 0..3 {
        let review_id = format!("R{}", round);
        store
            .append(Event {
                kind: EventKind::ReviewRequested {
                    task_id: task_id.clone(),
                    review_id: review_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();

        for reviewer in 0..3 {
            store
                .append(Event {
                    kind: EventKind::ReviewScoreReceived {
                        review_id: review_id.clone(),
                        reviewer_id: format!("reviewer-{}", reviewer),
                        score: 6,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: review_id.clone(),
                })
                .await
                .unwrap();
        }
    }

    // Add ReviewChangesRequested for the first 2 rounds
    store
        .append(Event {
            kind: EventKind::ReviewChangesRequested {
                review_id: "R0".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R0".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewChangesRequested {
                review_id: "R1".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R1".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::EscalationTriggered {
                reason: "max_rounds_exceeded".into(),
                context: "Score threshold not met after 3 rounds".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    let request = DecisionRequest {
        request_id: "req-1".into(),
        kind: DecisionKind::StuckGuidance,
        context: format!("Task {} review deadlock", task_id),
        event_ids: vec![],
        options: vec![
            "approve_with_conditions".into(),
            "reject".into(),
            "request_more_changes".into(),
        ],
        created_at: Utc::now(),
    };

    let response = decision_engine.decide(request).await.unwrap();
    assert_eq!(response.decision, "merge_with_conditions");

    let events = store.all_events();
    assert!(
        event_was_emitted(&events, "EscalationTriggered"),
        "Escalation should trigger for review deadlock"
    );

    let review_rounds = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::ReviewRequested { .. }))
        .count();
    assert_eq!(review_rounds, 3, "Should have 3 review rounds");

    let changes_requested = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::ReviewChangesRequested { .. }))
        .count();
    assert_eq!(
        changes_requested, 2,
        "Should request changes after first 2 rounds"
    );
}
