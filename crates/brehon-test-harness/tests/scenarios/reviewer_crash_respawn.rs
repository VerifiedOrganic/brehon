//! Test: Reviewer dies mid-review, entire panel respawned, review restarts
//!
//! Reviewer crashes during review
//! Entire panel killed
//! New panel spawned
//! Review restarts from round 1
//! Assert: panel respawn, review restarts

use std::sync::Arc;

use brehon_ports::{AgentGateway, EventStore};
use brehon_test_harness::{event_was_emitted, InMemoryEventStore, MockGateway};
use brehon_types::{AgentId, Event, EventKind, SessionSpec};
use chrono::Utc;

#[tokio::test]
async fn reviewer_crash_panel_respawn_review_restart() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());

    let task_id = "T001".to_string();
    let review_id_round1 = "R001".to_string();

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

    let reviewers = vec![
        AgentId::new("reviewer-1"),
        AgentId::new("reviewer-2"),
        AgentId::new("reviewer-3"),
    ];

    let mut reviewer_sessions = vec![];
    for reviewer_id in &reviewers {
        let session = gateway
            .spawn(SessionSpec::new(
                reviewer_id.clone(),
                "reviewer".into(),
                "/tmp/review".into(),
            ))
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::AgentSpawned {
                    agent_id: reviewer_id.as_str().to_string(),
                    session_id: session.as_str().to_string(),
                    role: "reviewer".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: reviewer_id.as_str().to_string(),
            })
            .await
            .unwrap();

        reviewer_sessions.push(session);
    }

    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.clone(),
                review_id: review_id_round1.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_round1.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id_round1.clone(),
                reviewer_id: reviewers[0].as_str().to_string(),
                score: 8,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_round1.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id_round1.clone(),
                reviewer_id: reviewers[1].as_str().to_string(),
                score: 7,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_round1.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::AgentDied {
                agent_id: reviewers[2].as_str().to_string(),
                session_id: reviewer_sessions[2].as_str().to_string(),
                reason: "crash_during_review".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: reviewers[2].as_str().to_string(),
        })
        .await
        .unwrap();

    for session in &reviewer_sessions {
        gateway.kill_session(session).await.unwrap();
    }

    for reviewer_id in &reviewers {
        store
            .append(Event {
                kind: EventKind::AgentDied {
                    agent_id: reviewer_id.as_str().to_string(),
                    session_id: format!("session-{}", reviewer_id.as_str()),
                    reason: "panel_killed".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: reviewer_id.as_str().to_string(),
            })
            .await
            .unwrap();
    }

    let review_id_round2 = "R002".to_string();
    let mut new_reviewer_sessions = vec![];

    for reviewer_id in &reviewers {
        let session = gateway
            .spawn(SessionSpec::new(
                reviewer_id.clone(),
                "reviewer".into(),
                "/tmp/review-round2".into(),
            ))
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::AgentSpawned {
                    agent_id: reviewer_id.as_str().to_string(),
                    session_id: session.as_str().to_string(),
                    role: "reviewer".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: reviewer_id.as_str().to_string(),
            })
            .await
            .unwrap();

        new_reviewer_sessions.push(session);
    }

    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.clone(),
                review_id: review_id_round2.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_round2.clone(),
        })
        .await
        .unwrap();

    for reviewer_id in &reviewers {
        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id_round2.clone(),
                    reviewer_id: reviewer_id.as_str().to_string(),
                    score: 8,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id_round2.clone(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::ReviewApproved {
                review_id: review_id_round2.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_round2.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::MergeCommitted {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let agent_deaths: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentDied { .. }))
        .collect();
    assert!(
        agent_deaths.len() >= 4,
        "Should have initial reviewer crash + panel kill events"
    );

    let agent_spawns = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentSpawned { role, .. } if role == "reviewer"))
        .count();
    assert_eq!(
        agent_spawns, 6,
        "Should spawn 3 reviewers twice (initial + respawn)"
    );

    let review_requests: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::ReviewRequested { .. }))
        .collect();
    assert_eq!(
        review_requests.len(),
        2,
        "Should have 2 review requests (round 1 + round 2)"
    );

    assert!(
        event_was_emitted(&events, "ReviewApproved"),
        "Review should be approved after panel respawn"
    );
    assert!(
        event_was_emitted(&events, "MergeCommitted"),
        "Task should merge after successful review"
    );
}

#[tokio::test]
async fn reviewer_panel_recovery_preserves_review_integrity() {
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

    let review_id_1 = "R001".to_string();
    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.clone(),
                review_id: review_id_1.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_1.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id_1.clone(),
                reviewer_id: "reviewer-1".into(),
                score: 8,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_1.clone(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::AgentDied {
                agent_id: "reviewer-2".into(),
                session_id: "session-r2".into(),
                reason: "crash".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "reviewer-2".into(),
        })
        .await
        .unwrap();

    let review_id_2 = "R002".to_string();
    store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.clone(),
                review_id: review_id_2.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_2.clone(),
        })
        .await
        .unwrap();

    for reviewer in ["reviewer-1", "reviewer-2", "reviewer-3"] {
        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id_2.clone(),
                    reviewer_id: reviewer.into(),
                    score: 8,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id_2.clone(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::ReviewApproved {
                review_id: review_id_2.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_2.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let first_round_scores: Vec<u8> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::ReviewScoreReceived {
                review_id: rid,
                score,
                ..
            } = &e.kind
            {
                if rid == "R001" {
                    Some(*score)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        first_round_scores.len(),
        1,
        "First round should have only 1 score before crash"
    );

    let events = store.all_events();
    let second_round_scores: Vec<u8> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::ReviewScoreReceived {
                review_id: rid,
                score,
                ..
            } = &e.kind
            {
                if rid == "R002" {
                    Some(*score)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        second_round_scores.len(),
        3,
        "Second round should have all 3 scores"
    );
}

#[tokio::test]
async fn reviewer_panel_all_new_after_crash() {
    let store = InMemoryEventStore::new();

    let original_reviewers = vec!["reviewer-a", "reviewer-b", "reviewer-c"];
    let new_reviewers = vec!["reviewer-x", "reviewer-y", "reviewer-z"];

    for reviewer in &original_reviewers {
        store
            .append(Event {
                kind: EventKind::AgentSpawned {
                    agent_id: reviewer.to_string(),
                    session_id: format!("session-{}", reviewer),
                    role: "reviewer".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: reviewer.to_string(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::AgentDied {
                agent_id: "reviewer-a".into(),
                session_id: "session-reviewer-a".into(),
                reason: "crash".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "reviewer-a".into(),
        })
        .await
        .unwrap();

    for reviewer in &new_reviewers {
        store
            .append(Event {
                kind: EventKind::AgentSpawned {
                    agent_id: reviewer.to_string(),
                    session_id: format!("session-{}", reviewer),
                    role: "reviewer".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: reviewer.to_string(),
            })
            .await
            .unwrap();
    }

    let events = store.all_events();

    let spawns: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::AgentSpawned { agent_id, role, .. } = &e.kind {
                if role == "reviewer" {
                    Some(agent_id.clone())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        spawns.len(),
        6,
        "Should have 6 total spawns (3 original + 3 new)"
    );
}
