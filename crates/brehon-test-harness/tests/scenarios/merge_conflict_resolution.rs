//! Test: Two approved branches conflict, conflict resolution task created
//!
//! Two tasks complete review
//! First merges successfully
//! Second has merge conflict
//! Conflict resolution task created
//! Assert: merge_conflict event, resolution task created

use std::sync::Arc;

use brehon_ports::{EventStore, GitOperations};
use brehon_test_harness::{
    event_was_emitted, mock_git::FileContent, FakeGitOperations, InMemoryEventStore, MockGateway,
};
use brehon_types::EventKind;
use chrono::Utc;

#[tokio::test]
async fn merge_conflict_creates_resolution_task() {
    let store = Arc::new(InMemoryEventStore::new());
    let git = Arc::new(FakeGitOperations::new());
    let _gateway = Arc::new(MockGateway::new());

    let task1_id = "T001".to_string();
    let task2_id = "T002".to_string();
    let branch1 = "feature/task-1".to_string();
    let branch2 = "feature/task-2".to_string();

    for task_id in [&task1_id, &task2_id] {
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
    }

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskAssigned {
                task_id: task1_id.clone(),
                agent_id: "worker-1".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task1_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskAssigned {
                task_id: task2_id.clone(),
                agent_id: "worker-2".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: task2_id.clone(),
        })
        .await
        .unwrap();

    for task_id in [&task1_id, &task2_id] {
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

        let review_id = format!("R-{}", task_id);
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

        for reviewer in ["reviewer-1", "reviewer-2"] {
            store
                .append(brehon_types::Event {
                    kind: EventKind::ReviewScoreReceived {
                        review_id: review_id.clone(),
                        reviewer_id: reviewer.into(),
                        score: 8,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: review_id.clone(),
                })
                .await
                .unwrap();
        }

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
    }

    git.create_branch(&branch1);
    git.create_branch(&branch2);

    let conflict_files = vec!["src/auth.rs".into(), "src/config.rs".into()];
    git.add_conflict_files(&branch2, conflict_files.clone());

    store
        .append(brehon_types::Event {
            kind: EventKind::MergePrepared {
                task_id: task1_id.clone(),
                branch: branch1.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task1_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergeCommitted {
                task_id: task1_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task1_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergePrepared {
                task_id: task2_id.clone(),
                branch: branch2.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: task2_id.clone(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergeAborted {
                task_id: task2_id.clone(),
                reason: format!("conflict with merged task {}", task1_id),
            },
            timestamp: Utc::now(),
            aggregate_id: task2_id.clone(),
        })
        .await
        .unwrap();

    let resolution_task_id = "T003".to_string();
    store
        .append(brehon_types::Event {
            kind: EventKind::TaskCreated {
                task_id: resolution_task_id.clone(),
            },
            timestamp: Utc::now(),
            aggregate_id: resolution_task_id.clone(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "MergeAborted"),
        "Merge should be aborted for conflicting branches"
    );

    let merge_aborted_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::MergeAborted { .. }))
        .collect();
    assert_eq!(merge_aborted_events.len(), 1, "Should have one merge abort");

    if let EventKind::MergeAborted { reason, .. } = &merge_aborted_events[0].kind {
        assert!(
            reason.contains("conflict"),
            "Abort reason should mention conflict"
        );
    }

    let merge_committed_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::MergeCommitted { .. }))
        .collect();
    assert_eq!(
        merge_committed_events.len(),
        1,
        "First task should merge successfully"
    );

    assert!(
        event_was_emitted(&events, "TaskCreated"),
        "Resolution task should be created"
    );

    let task_created_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::TaskCreated { .. }))
        .count();
    assert_eq!(
        task_created_count, 3,
        "Should have 3 tasks (2 original + 1 resolution)"
    );
}

#[tokio::test]
async fn merge_conflict_detection_with_file_overlap() {
    let _store = InMemoryEventStore::new();
    let git = FakeGitOperations::new();

    git.create_branch("feature/a");
    git.create_branch("feature/b");

    let mut main_files = std::collections::HashMap::new();
    main_files.insert(
        "src/auth.rs".into(),
        FileContent {
            content: "main content".into(),
            lines_added: 10,
            lines_removed: 5,
        },
    );
    git.set_branch_files("main", main_files);

    let mut branch_files = std::collections::HashMap::new();
    branch_files.insert(
        "src/auth.rs".into(),
        FileContent {
            content: "branch content".into(),
            lines_added: 15,
            lines_removed: 2,
        },
    );
    git.set_branch_files("feature/b", branch_files);

    let conflicts = git.has_conflicts("feature/b", "main").await.unwrap();
    assert!(!conflicts.is_empty(), "Should detect file conflict");
}

#[tokio::test]
async fn merge_conflict_resolution_workflow() {
    let store = InMemoryEventStore::new();

    let original_task = "T001";
    let resolution_task = "T-RESOLVE-001";

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskCreated {
                task_id: original_task.into(),
            },
            timestamp: Utc::now(),
            aggregate_id: original_task.into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergeAborted {
                task_id: original_task.into(),
                reason: "conflict: src/main.rs".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: original_task.into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskCreated {
                task_id: resolution_task.into(),
            },
            timestamp: Utc::now(),
            aggregate_id: resolution_task.into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskAssigned {
                task_id: resolution_task.into(),
                agent_id: "resolver-1".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: resolution_task.into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::TaskCompleted {
                task_id: resolution_task.into(),
            },
            timestamp: Utc::now(),
            aggregate_id: resolution_task.into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewRequested {
                task_id: resolution_task.into(),
                review_id: "R-RESOLVE".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R-RESOLVE".into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::ReviewApproved {
                review_id: "R-RESOLVE".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "R-RESOLVE".into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergeCommitted {
                task_id: resolution_task.into(),
            },
            timestamp: Utc::now(),
            aggregate_id: resolution_task.into(),
        })
        .await
        .unwrap();

    store
        .append(brehon_types::Event {
            kind: EventKind::MergeCommitted {
                task_id: original_task.into(),
            },
            timestamp: Utc::now(),
            aggregate_id: original_task.into(),
        })
        .await
        .unwrap();

    let events = store.all_events();

    assert!(
        event_was_emitted(&events, "MergeAborted"),
        "Original merge should abort"
    );
    assert!(
        event_was_emitted(&events, "TaskCreated"),
        "Resolution task should be created"
    );
    assert!(
        event_was_emitted(&events, "MergeCommitted"),
        "Both tasks should eventually merge"
    );

    let merge_committed_count = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::MergeCommitted { .. }))
        .count();
    assert_eq!(
        merge_committed_count, 2,
        "Both tasks should merge after resolution"
    );
}
