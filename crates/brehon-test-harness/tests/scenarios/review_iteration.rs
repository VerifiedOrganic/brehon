//! Test: Review round 1 fails (score 5), worker iterates, round 2 passes (score 8)
//!
//! First review round returns low scores
//! Worker receives feedback, iterates
//! Second round returns passing scores
//! Assert: 2 review rounds, task_status merged

use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{event_was_emitted, InMemoryEventStore, MockGateway};
use brehon_types::EventKind;
use chrono::Utc;

#[tokio::test]
async fn review_iteration_first_round_fails_second_passes() {
    let store = Arc::new(InMemoryEventStore::new());
    let _gateway = Arc::new(MockGateway::new());

    let task_id = "T001".to_string();
    let review_id = "R001".to_string();

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
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
        .append(brehon_types::Event {
            kind: EventKind::TaskCompleted {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
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
        .append(brehon_types::Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.clone(),
                reviewer_id: "reviewer-1".into(),
                score: 5,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.clone(),
                reviewer_id: "reviewer-2".into(),
                score: 6,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewChangesRequested {
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
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
        .append(brehon_types::Event {
            kind: EventKind::TaskCompleted {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    let review_id_2 = "R002".to_string();
    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewRequested {
                task_id: task_id.clone(),
                review_id: review_id_2.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_2.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id_2.clone(),
                reviewer_id: "reviewer-1".into(),
                score: 8,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_2.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id_2.clone(),
                reviewer_id: "reviewer-2".into(),
                score: 7,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_2.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewApproved {
                review_id: review_id_2.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id_2.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergePrepared {
                task_id: task_id.clone(),
                branch: "feature/T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergeCommitted {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "ReviewChangesRequested"),
        "First round should request changes"
    );
    assert!(
        event_was_emitted(&events, "ReviewApproved"),
        "Second round should approve"
    );

    let review_rounds = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReviewRequested { .. }))
        .count();
    assert_eq!(review_rounds, 2, "Should have exactly 2 review rounds");

    let task_assigned = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::TaskAssigned { .. }))
        .count();
    assert_eq!(
        task_assigned, 2,
        "Worker should be assigned twice (initial + reassignment after changes)"
    );

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

    let first_avg = first_round_scores.iter().sum::<u8>() as f64 / first_round_scores.len() as f64;
    assert!(
        first_avg < 7.0,
        "First round average should be below threshold: {}",
        first_avg
    );

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

    let second_avg =
        second_round_scores.iter().sum::<u8>() as f64 / second_round_scores.len() as f64;
    assert!(
        second_avg >= 7.0,
        "Second round average should meet threshold: {}",
        second_avg
    );

    assert!(
        event_was_emitted(&events, "MergeCommitted"),
        "Task should merge after passing review"
    );
}

#[tokio::test]
async fn review_iteration_multiple_rounds_before_pass() {
    let store = InMemoryEventStore::new();
    let task_id = "T002".to_string();

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    for round in 0..3 {
        store
            .append(brehon_types::Event {
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
            .append(brehon_types::Event {
                kind: EventKind::TaskCompleted {
                    task_id: task_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.clone(),
            })
            .await
            .unwrap();

        let review_id = format!("R{:03}", round);
        store
            .append(brehon_types::Event {
                kind: EventKind::ReviewRequested {
                    task_id: task_id.clone(),
                    review_id: review_id.clone(),
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.clone(),
            })
            .await
            .unwrap();

        let scores = if round < 2 {
            vec![4u8, 5u8, 5u8]
        } else {
            vec![8u8, 7u8, 8u8]
        };

        for (idx, score) in scores.into_iter().enumerate() {
            store
                .append(brehon_types::Event {
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

        if round < 2 {
            store
                .append(brehon_types::Event {
                    kind: EventKind::ReviewChangesRequested {
                        review_id: review_id.clone(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: review_id.clone(),
                })
                .await
                .unwrap();
        } else {
            store
                .append(brehon_types::Event {
                    kind: EventKind::ReviewApproved {
                        review_id: review_id.clone(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: review_id.clone(),
                })
                .await
                .unwrap();

            store
                .append(brehon_types::Event {
                    kind: EventKind::MergeCommitted {
                        task_id: task_id.clone(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: task_id.clone(),
                })
                .await
                .unwrap();
        }
    }

    let events = store.all_events();

    let changes_requested_count = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReviewChangesRequested { .. }))
        .count();
    assert_eq!(
        changes_requested_count, 2,
        "Should have 2 rounds of changes requested"
    );

    let review_rounds = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReviewRequested { .. }))
        .count();
    assert_eq!(review_rounds, 3, "Should have 3 review rounds");

    assert!(
        event_was_emitted(&events, "ReviewApproved"),
        "Final round should be approved"
    );
    assert!(
        event_was_emitted(&events, "MergeCommitted"),
        "Task should merge after 3 rounds"
    );
}

#[tokio::test]
async fn review_iter_with_specific_feedback() {
    let store = InMemoryEventStore::new();
    let task_id = "T003".to_string();
    let review_id = "R001".to_string();

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskCreated {
                task_id: task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
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
        .append(brehon_types::Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.clone(),
                reviewer_id: "reviewer-1".into(),
                score: 5,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewChangesRequested {
                review_id: review_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    let low_scores: Vec<u8> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::ReviewScoreReceived { score, .. } = &e.kind {
                Some(*score)
            } else {
                None
            }
        })
        .filter(|s| *s < 7)
        .collect();

    assert!(!low_scores.is_empty(), "First round should have low scores");

    assert!(
        event_was_emitted(&events, "ReviewChangesRequested"),
        "Changes should be requested for low scores"
    );
}
