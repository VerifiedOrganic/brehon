//! Test: Main moves during review, overlapping files changed, review invalidated
//!
//! Review starts on branch
//! Main branch receives commits touching same files
//! Stale detection fires
//! Review invalidated
//! Assert: stale_review event

use std::sync::Arc;

use brehon_ports::{EventStore, GitOperations};
use brehon_test_harness::{
    event_was_emitted, mock_git::FileContent, FakeGitOperations, InMemoryEventStore, MockGateway,
};
use brehon_types::{Event, EventKind};
use chrono::Utc;

#[tokio::test]
async fn stale_review_detection_invalidates_review() {
    let store = Arc::new(InMemoryEventStore::new());
    let git = Arc::new(FakeGitOperations::new());
    let _gateway = Arc::new(MockGateway::new());

    let task_id = "T001".to_string();
    let branch = "feature/T001";
    let review_id = "R001".to_string();

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

    git.create_branch(branch);

    use std::collections::HashMap;
    let mut branch_files = HashMap::new();
    branch_files.insert(
        "src/auth.rs".into(),
        FileContent {
            content: "fn validate() { }".into(),
            lines_added: 5,
            lines_removed: 0,
        },
    );
    branch_files.insert(
        "src/config.rs".into(),
        FileContent {
            content: "struct Config { }".into(),
            lines_added: 10,
            lines_removed: 0,
        },
    );
    git.set_branch_files(branch, branch_files);

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
                score: 8,
            },
            timestamp: Utc::now(),
            aggregate_id: review_id.clone(),
        })
        .await
        .unwrap();

    git.create_branch("main-updated");
    let mut main_files = HashMap::new();
    main_files.insert(
        "src/auth.rs".into(),
        FileContent {
            content: "fn validate_token() { }".into(),
            lines_added: 15,
            lines_removed: 0,
        },
    );
    git.set_branch_files("main", main_files);

    store
        .append(Event {
            kind: EventKind::MergeCommitted {
                task_id: "OTHER-TASK".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "OTHER-TASK".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::MergeAborted {
                task_id: task_id.clone(),
                reason: "review_stale_conflict".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task_id.clone(),
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

    for reviewer_idx in 0..3 {
        store
            .append(Event {
                kind: EventKind::ReviewScoreReceived {
                    review_id: review_id_2.clone(),
                    reviewer_id: format!("reviewer-{}", reviewer_idx),
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

    assert!(
        event_was_emitted(&events, "MergeAborted"),
        "Stale review should cause merge abort"
    );

    let review_requests: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::ReviewRequested { .. }))
        .collect();
    assert_eq!(
        review_requests.len(),
        2,
        "Should have 2 review requests (original + re-review)"
    );

    let reviews_approved: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::ReviewApproved { .. }))
        .collect();
    assert_eq!(
        reviews_approved.len(),
        1,
        "Only second review should be approved"
    );

    assert!(
        event_was_emitted(&events, "MergeCommitted"),
        "Task should eventually merge after re-review"
    );
}

#[tokio::test]
async fn stale_review_different_files_no_invalidation() {
    let _store = InMemoryEventStore::new();
    let git = FakeGitOperations::new();

    let _task_id = "T002";
    let branch = "feature/T002";

    git.create_branch(branch);
    let mut branch_files = std::collections::HashMap::new();
    branch_files.insert(
        "src/utils.rs".into(),
        FileContent {
            content: "fn helper() { }".into(),
            lines_added: 5,
            lines_removed: 0,
        },
    );
    git.set_branch_files(branch, branch_files);

    let mut main_files = std::collections::HashMap::new();
    main_files.insert(
        "src/auth.rs".into(),
        FileContent {
            content: "fn login() { }".into(),
            lines_added: 10,
            lines_removed: 0,
        },
    );
    git.set_branch_files("main", main_files);

    let conflicts = git.has_conflicts(branch, "main").await.unwrap();
    assert!(
        conflicts.is_empty(),
        "No conflicts when different files changed"
    );
}

#[tokio::test]
async fn multiple_stale_reviews_concurrent_merges() {
    let store = InMemoryEventStore::new();

    let tasks = vec!["T001", "T002", "T003"];
    let conflicting_file = "src/shared.rs";

    for task_id in &tasks {
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

        let review_id = format!("R-{}", task_id);
        store
            .append(Event {
                kind: EventKind::ReviewRequested {
                    task_id: task_id.to_string(),
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
            kind: EventKind::MergeCommitted {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    for task_id in &["T002", "T003"] {
        store
            .append(Event {
                kind: EventKind::MergeAborted {
                    task_id: task_id.to_string(),
                    reason: format!("stale: {} modified by T001", conflicting_file),
                },
                timestamp: Utc::now(),
                aggregate_id: task_id.to_string(),
            })
            .await
            .unwrap();
    }

    let events = store.all_events();

    let merge_aborted_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::MergeAborted { .. }))
        .count();
    assert_eq!(
        merge_aborted_count, 2,
        "T002 and T003 should have merge aborts"
    );

    let merge_committed_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::MergeCommitted { .. }))
        .count();
    assert_eq!(
        merge_committed_count, 1,
        "Only T001 should merge successfully"
    );
}

#[tokio::test]
async fn stale_review_rebase_required() {
    let store = InMemoryEventStore::new();
    let git = FakeGitOperations::new();

    git.create_branch("feature/stale");
    git.create_branch("main");

    let mut feature_files = std::collections::HashMap::new();
    feature_files.insert(
        "src/api.rs".into(),
        FileContent {
            content: "fn endpoint() { }".into(),
            lines_added: 20,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature/stale", feature_files);

    store
        .append(Event {
            kind: EventKind::MergeAborted {
                task_id: "T001".into(),
                reason: "stale: main has new commits on src/api.rs".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: "T001".into(),
                agent_id: "worker-rebase".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::MergePrepared {
                task_id: "T001".into(),
                branch: "feature/stale-rebased".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    store
        .append(Event {
            kind: EventKind::MergeCommitted {
                task_id: "T001".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T001".into(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "MergeAborted"),
        "Initial merge should abort"
    );
    assert!(
        event_was_emitted(&events, "MergePrepared"),
        "Rebase should be prepared"
    );
    assert!(
        event_was_emitted(&events, "MergeCommitted"),
        "Final merge should succeed"
    );
}
