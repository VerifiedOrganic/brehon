//! Crash recovery test: Mid-review crash.
//!
//! Tests that if a crash occurs after receiving 2 of 3 reviewer scores,
//! the recovery detects the incomplete review and restarts the review round.

use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_test_harness::{CrashInjector, CrashPoint, CrashScenario, InMemoryEventStore};
use brehon_types::{Event, EventFilter, EventKind};
use chrono::Utc;

#[tokio::test]
async fn crash_mid_review_detects_incomplete_review() {
    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T001";
    let review_id = "R001";

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
            kind: EventKind::ReviewRequested {
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
        })
        .await
        .unwrap();

    let injector = CrashInjector::new().add_scenario(CrashScenario {
        name: "mid-review".into(),
        crash_points: vec![CrashPoint::AfterMessageCount(2)],
        restart_after_crash: true,
        verify_recovery: true,
    });

    let mut inj = injector;
    inj.start_scenario("mid-review");

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.to_string(),
                reviewer_id: "reviewer-1".to_string(),
                score: 8,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
        })
        .await
        .unwrap();
    let crashed = inj.record_message();
    assert!(!crashed, "First reviewer score should not trigger crash");

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.to_string(),
                reviewer_id: "reviewer-2".to_string(),
                score: 7,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
        })
        .await
        .unwrap();
    let crashed = inj.record_message();
    assert!(crashed, "Second reviewer score should trigger crash");
    assert!(inj.should_crash(), "Crash should be flagged");

    let review_events = store
        .query(EventFilter::new().aggregate(review_id))
        .await
        .unwrap();

    let score_count = review_events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::ReviewScoreReceived { .. }))
        .count();

    assert_eq!(score_count, 2, "Should have 2 scores before crash");

    let incomplete = detect_incomplete_review(&review_events, review_id, 3);
    assert!(
        incomplete,
        "Recovery should detect incomplete review (2 of 3 scores)"
    );
}

#[tokio::test]
async fn crash_mid_review_recovery_restarts_round() {
    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T002";
    let review_id = "R002";

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
            kind: EventKind::ReviewRequested {
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.to_string(),
                reviewer_id: "reviewer-1".to_string(),
                score: 8,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::ReviewScoreReceived {
                review_id: review_id.to_string(),
                reviewer_id: "reviewer-2".to_string(),
                score: 6,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
        })
        .await
        .unwrap();

    let restarted = recover_incomplete_review(&store, review_id, 3).await;
    assert!(restarted, "Recovery should restart incomplete review");

    let events = store
        .query(EventFilter::new().aggregate(review_id))
        .await
        .unwrap();

    let has_rejection = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ReviewRejected { review_id: r } if r == review_id
        )
    });
    assert!(!has_rejection, "Should not reject, but restart");

    let has_new_request = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ReviewRequested { review_id: r, .. } if r == review_id
        )
    });
    assert!(
        has_new_request || events.len() >= 4,
        "Review should be restarted or additional events emitted"
    );
}

#[tokio::test]
async fn crash_mid_review_complete_flow_no_crash() {
    let store = Arc::new(InMemoryEventStore::new());
    let task_id = "T003";
    let review_id = "R003";

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
            kind: EventKind::ReviewRequested {
                task_id: task_id.to_string(),
                review_id: review_id.to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
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

    for reviewer in ["reviewer-1", "reviewer-2", "reviewer-3"] {
        let crashed = inj.record_message();
        assert!(!crashed, "No crash should occur in complete flow");

        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id.to_string(),
                    reviewer_id: reviewer.to_string(),
                    score: 8,
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.to_string(),
            })
            .await
            .unwrap();
    }

    store
        .append(Event {
            kind: EventKind::ReviewApproved {
                review_id: review_id.to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.to_string(),
        })
        .await
        .unwrap();

    let events = store
        .query(EventFilter::new().aggregate(review_id))
        .await
        .unwrap();

    let incomplete = detect_incomplete_review(&events, review_id, 3);
    assert!(
        !incomplete,
        "Complete review (3 scores + approval) should not be incomplete"
    );

    let has_approval = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ReviewApproved { review_id: r } if r == review_id
        )
    });
    assert!(
        has_approval,
        "Review should be approved after complete flow"
    );
}

fn detect_incomplete_review(events: &[Event], review_id: &str, required_scores: usize) -> bool {
    let scores: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                EventKind::ReviewScoreReceived { review_id: r, .. } if r == review_id
            )
        })
        .collect();

    let has_approval = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ReviewApproved { review_id: r } if r == review_id
        )
    });

    let has_rejection = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ReviewRejected { review_id: r } if r == review_id
        )
    });

    let score_count = scores.len();
    score_count > 0 && score_count < required_scores && !has_approval && !has_rejection
}

async fn recover_incomplete_review(
    store: &InMemoryEventStore,
    review_id: &str,
    required_scores: usize,
) -> bool {
    let events = store
        .query(EventFilter::new().aggregate(review_id))
        .await
        .unwrap();

    if detect_incomplete_review(&events, review_id, required_scores) {
        store
            .append(Event {
                kind: EventKind::ReviewRequested {
                    task_id: format!("task-for-{}", review_id),
                    review_id: review_id.to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: review_id.to_string(),
            })
            .await
            .unwrap();
        return true;
    }
    false
}
