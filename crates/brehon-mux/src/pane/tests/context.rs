use crate::pane::Pane;
use std::time::Instant;

#[test]
fn test_task_context_snapshot_is_terminal_for_merged() {
    use crate::pane::TaskContextSnapshot;
    use brehon_types::task::TaskStatus;

    let snapshot = TaskContextSnapshot {
        task_id: "T-123".to_string(),
        title: "Test task".to_string(),
        status: TaskStatus::Merged,
        completion_mode: Some("merge".to_string()),
        merge_target: Some("main".to_string()),
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: Instant::now(),
    };

    assert!(snapshot.is_terminal(), "Merged tasks should be terminal");
}

#[test]
fn test_task_context_snapshot_non_terminal_for_in_progress() {
    use crate::pane::TaskContextSnapshot;
    use brehon_types::task::TaskStatus;

    let snapshot = TaskContextSnapshot {
        task_id: "T-456".to_string(),
        title: "Active task".to_string(),
        status: TaskStatus::InProgress,
        completion_mode: Some("merge".to_string()),
        merge_target: Some("main".to_string()),
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: Instant::now(),
    };

    assert!(
        !snapshot.is_terminal(),
        "InProgress tasks should not be terminal"
    );
}

#[test]
fn test_task_context_snapshot_non_terminal_for_assigned() {
    use crate::pane::TaskContextSnapshot;
    use brehon_types::task::TaskStatus;

    let snapshot = TaskContextSnapshot {
        task_id: "T-789".to_string(),
        title: "Assigned task".to_string(),
        status: TaskStatus::Assigned,
        completion_mode: None,
        merge_target: None,
        parent_id: Some("E-100".to_string()),
        epic_branch: Some("epic/T-100".to_string()),
        epic_worktree: Some(std::path::PathBuf::from("/tmp/worktrees/epic-T-100")),
        blocked_reason: None,
        updated_at: Instant::now(),
    };

    assert!(
        !snapshot.is_terminal(),
        "Assigned tasks should not be terminal"
    );
}

#[test]
fn test_task_context_snapshot_blocked_has_reason() {
    use crate::pane::{TaskBlockedReason, TaskContextSnapshot};
    use brehon_types::task::TaskStatus;

    let snapshot = TaskContextSnapshot {
        task_id: "T-blocked".to_string(),
        title: "Blocked task".to_string(),
        status: TaskStatus::Blocked,
        completion_mode: Some("merge".to_string()),
        merge_target: Some("main".to_string()),
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: Some(TaskBlockedReason {
            blocker_task_id: Some("T-dependency".to_string()),
            summary: Some("Waiting on dependency".to_string()),
        }),
        updated_at: Instant::now(),
    };

    assert!(
        !snapshot.is_terminal(),
        "Blocked tasks should not be terminal"
    );
    assert!(snapshot.blocked_reason.is_some());
    let reason = snapshot.blocked_reason.as_ref().unwrap();
    assert_eq!(reason.blocker_task_id.as_deref(), Some("T-dependency"));
    assert_eq!(reason.summary.as_deref(), Some("Waiting on dependency"));
}

#[test]
fn test_pane_set_and_get_task_context() {
    use crate::pane::TaskContextSnapshot;
    use brehon_types::task::TaskStatus;

    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    assert!(
        pane.task_context().is_none(),
        "new pane should have no task context"
    );

    let snapshot = TaskContextSnapshot {
        task_id: "T-test".to_string(),
        title: "Test task".to_string(),
        status: TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: Instant::now(),
    };

    pane.set_task_context(snapshot.clone());
    assert!(
        pane.task_context().is_some(),
        "pane should have task context after set"
    );

    let ctx = pane.task_context().unwrap();
    assert_eq!(ctx.task_id, "T-test");
    assert_eq!(ctx.title, "Test task");

    pane.clear_task_context();
    assert!(
        pane.task_context().is_none(),
        "pane should have no task context after clear"
    );
}

#[test]
fn test_pane_task_context_mut() {
    use crate::pane::TaskContextSnapshot;
    use brehon_types::task::TaskStatus;

    let mut pane = Pane::director("test", 24, 80).expect("create pane");

    let snapshot = TaskContextSnapshot {
        task_id: "T-mut".to_string(),
        title: "Original title".to_string(),
        status: TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: Instant::now(),
    };

    pane.set_task_context(snapshot);

    if let Some(ctx) = pane.task_context_mut() {
        ctx.title = "Updated title".to_string();
    }

    assert_eq!(pane.task_context().unwrap().title, "Updated title");
}

#[test]
fn test_pane_task_context_returns_none_when_not_set() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    assert!(pane.task_context().is_none());
    assert!(pane.task_context_mut().is_none());
}

#[test]
fn test_pane_set_and_get_review_context() {
    use crate::pane::ReviewContextSnapshot;

    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    assert!(
        pane.review_context().is_none(),
        "new pane should have no review context"
    );

    let snapshot = ReviewContextSnapshot {
        review_id: "R-1".to_string(),
        task_id: "T-1".to_string(),
        round: 1,
        panel_total: 3,
        panel_done: 1,
        verdict: None,
        score: None,
        findings_summary: None,
        updated_at: Instant::now(),
    };

    pane.set_review_context(snapshot.clone());
    assert!(pane.review_context().is_some());

    let ctx = pane.review_context().unwrap();
    assert_eq!(ctx.review_id, "R-1");
    assert_eq!(ctx.task_id, "T-1");
    assert_eq!(ctx.panel_done, 1);

    pane.clear_review_context();
    assert!(pane.review_context().is_none());
}

#[test]
fn test_pane_review_context_mut() {
    use crate::pane::ReviewContextSnapshot;

    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    pane.set_review_context(ReviewContextSnapshot {
        review_id: "R-mut".to_string(),
        task_id: "T-mut".to_string(),
        round: 2,
        panel_total: 3,
        panel_done: 3,
        verdict: Some("approve".to_string()),
        score: Some(9),
        findings_summary: Some("ok".to_string()),
        updated_at: Instant::now(),
    });

    if let Some(ctx) = pane.review_context_mut() {
        ctx.findings_summary = Some("updated".to_string());
    }

    assert_eq!(
        pane.review_context()
            .and_then(|ctx| ctx.findings_summary.as_deref()),
        Some("updated")
    );
}

#[test]
fn test_task_context_snapshot_from_task_with_details() {
    use crate::pane::{TaskBlockedReason, TaskContextDetails, TaskContextSnapshot};
    use brehon_types::task::{Priority, Task, TaskId, TaskStatus};
    use chrono::Utc;

    let task = Task {
        id: TaskId::new("T-real"),
        title: "Real task context".to_string(),
        description: "Render real task context in pane".to_string(),
        status: TaskStatus::Blocked,
        priority: Priority::High,
        assignee: Some("worker-1".to_string()),
        dependencies: vec![TaskId::new("T-base")],
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let snapshot = TaskContextSnapshot::from_task(
        &task,
        TaskContextDetails {
            completion_mode: Some("merge".to_string()),
            merge_target: Some("epic/feature".to_string()),
            parent_id: Some("E-feature".to_string()),
            epic_branch: Some("epic/feature".to_string()),
            epic_worktree: Some(std::path::PathBuf::from("/tmp/worktrees/feature")),
            blocked_reason: Some(TaskBlockedReason {
                blocker_task_id: Some("T-base".to_string()),
                summary: Some("Waiting on shared abstraction".to_string()),
            }),
        },
    );

    assert_eq!(snapshot.task_id, "T-real");
    assert_eq!(snapshot.title, "Real task context");
    assert_eq!(snapshot.status, TaskStatus::Blocked);
    assert_eq!(snapshot.epic_branch.as_deref(), Some("epic/feature"));
    assert_eq!(
        snapshot.epic_worktree.as_deref(),
        Some(std::path::Path::new("/tmp/worktrees/feature"))
    );
    let blocked = snapshot.blocked_reason.as_ref().expect("blocked reason");
    assert_eq!(blocked.blocker_task_id.as_deref(), Some("T-base"));
    assert_eq!(
        blocked.summary.as_deref(),
        Some("Waiting on shared abstraction")
    );
}
