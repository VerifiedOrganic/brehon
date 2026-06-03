use std::collections::HashMap;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use brehon_ports::{ClaimRequest, RunStore};
use brehon_test_harness::InMemoryRunStore;
use brehon_types::{
    ClaimGeneration, ClaimOwner, EventKind, RunId, RunRecord, RunRole, RunStatus, SessionId,
    TaskId, TaskStatus,
};

use crate::reconciler::{
    Reconciler, ReconcilerConfig, ReconciliationInput, RunRepairReason, TaskRepairReason,
    TaskUpdateAction, WorkerCleanupAction, WorkerSnapshotEntry,
};
use crate::task_board::TaskEntry;
use crate::worker_pool::{WorkerId, WorkerKind};

fn task(id: &str, status: TaskStatus, assignee: Option<&str>, session: Option<&str>) -> TaskEntry {
    let mut task = TaskEntry::new(TaskId::new(id), format!("Task {id}"), String::new());
    task.status = status;
    task.assignee = assignee.map(str::to_string);
    task.session_id = session.map(str::to_string);
    task
}

fn worker(
    id: &str,
    session: &str,
    assigned_task: Option<&str>,
    is_alive: bool,
) -> WorkerSnapshotEntry {
    WorkerSnapshotEntry {
        worker_id: WorkerId::new(id),
        kind: WorkerKind::Worker,
        session_id: SessionId::new(session),
        assigned_task: assigned_task.map(TaskId::new),
        is_alive,
    }
}

fn input(
    now: DateTime<Utc>,
    tasks: Vec<TaskEntry>,
    workers: Vec<WorkerSnapshotEntry>,
    active_runs: Vec<RunRecord>,
) -> ReconciliationInput {
    ReconciliationInput {
        now,
        tasks,
        workers,
        dependencies: HashMap::new(),
        git_ops: None,
        active_runs,
    }
}

fn run(
    id: &str,
    task_id: &str,
    role: RunRole,
    status: RunStatus,
    session: Option<&str>,
    lease_expires_at: Option<DateTime<Utc>>,
) -> RunRecord {
    let now = Utc::now();
    let mut record = RunRecord::new(RunId::new(id), TaskId::new(task_id), role, now);
    record.status = status;
    record.claim_generation = ClaimGeneration::new(1);
    record.claim_owner = Some(ClaimOwner::new("worker-1"));
    record.session_id = session.map(SessionId::new);
    record.lease_expires_at = lease_expires_at;
    record
}

#[test]
fn reconciler_dead_worker_unassigns_worker_owned_task() {
    let now = Utc::now();
    let plan = Reconciler::new().reconcile(input(
        now,
        vec![task(
            "T001",
            TaskStatus::InProgress,
            Some("worker-1"),
            Some("session-1"),
        )],
        vec![worker("worker-1", "session-1", Some("T001"), false)],
        vec![],
    ));

    assert!(matches!(
        plan.task_updates.as_slice(),
        [TaskUpdateAction::UnassignTask {
            task_id,
            reason: TaskRepairReason::DeadWorker,
            ..
        }] if task_id.as_str() == "T001"
    ));
    assert!(matches!(
        plan.worker_cleanup_actions.as_slice(),
        [WorkerCleanupAction::HandleWorkerDeath { session_id }]
            if session_id.as_str() == "session-1"
    ));
}

#[test]
fn reconciler_dead_worker_preserves_review_owned_task() {
    for status in [TaskStatus::InReview, TaskStatus::Approved] {
        let now = Utc::now();
        let plan = Reconciler::new().reconcile(input(
            now,
            vec![task("T001", status, Some("worker-1"), Some("session-1"))],
            vec![worker("worker-1", "session-1", Some("T001"), false)],
            vec![],
        ));

        assert!(
            plan.task_updates.is_empty(),
            "review-owned status {status:?} must not be unassigned"
        );
        assert!(matches!(
            plan.worker_cleanup_actions.as_slice(),
            [WorkerCleanupAction::HandleWorkerDeath { session_id }]
                if session_id.as_str() == "session-1"
        ));
    }
}

#[tokio::test]
async fn reconciler_expired_claim_renews_alive_operation_from_run_store() {
    let now = Utc::now();
    let store = InMemoryRunStore::new();
    let record = RunRecord::new(
        RunId::new("run-1"),
        TaskId::new("T001"),
        RunRole::Worker,
        now,
    );
    store.create_run(record).await.unwrap();
    store
        .claim_run(ClaimRequest::new(
            RunId::new("run-1"),
            ClaimOwner::new("worker-1"),
            Some(SessionId::new("session-1")),
            now - ChronoDuration::minutes(5),
            now - ChronoDuration::minutes(1),
        ))
        .await
        .unwrap();

    let plan = Reconciler::new()
        .reconcile_with_run_store(
            &store,
            input(
                now,
                vec![task(
                    "T001",
                    TaskStatus::InProgress,
                    Some("worker-1"),
                    Some("session-1"),
                )],
                vec![worker("worker-1", "session-1", Some("T001"), true)],
                vec![],
            ),
        )
        .await
        .unwrap();

    assert_eq!(plan.run_renewals.len(), 1);
    assert_eq!(plan.run_renewals[0].run.run_id.as_str(), "run-1");
    assert!(plan.run_failures.is_empty());
    assert!(plan.run_releases.is_empty());
}

#[test]
fn reconciler_expired_claim_releases_dead_unstarted_session() {
    let now = Utc::now();
    let active_run = run(
        "run-1",
        "T001",
        RunRole::Worker,
        RunStatus::Claimed,
        Some("session-1"),
        Some(now - ChronoDuration::seconds(1)),
    );

    let plan = Reconciler::new().reconcile(input(
        now,
        vec![task(
            "T001",
            TaskStatus::Assigned,
            Some("worker-1"),
            Some("session-1"),
        )],
        vec![worker("worker-1", "session-1", Some("T001"), false)],
        vec![active_run],
    ));

    assert_eq!(plan.run_releases.len(), 1);
    assert_eq!(plan.run_releases[0].reason, RunRepairReason::ExpiredClaim);
}

#[test]
fn reconciler_expired_claim_fails_running_dead_session() {
    let now = Utc::now();
    let active_run = run(
        "run-1",
        "T001",
        RunRole::Worker,
        RunStatus::Running,
        Some("session-1"),
        Some(now - ChronoDuration::seconds(1)),
    );

    let plan = Reconciler::new().reconcile(input(
        now,
        vec![task(
            "T001",
            TaskStatus::InProgress,
            Some("worker-1"),
            Some("session-1"),
        )],
        vec![worker("worker-1", "session-1", Some("T001"), false)],
        vec![active_run],
    ));

    assert_eq!(plan.run_failures.len(), 1);
    assert_eq!(
        plan.run_failures[0].reason,
        RunRepairReason::ExpiredRunningClaim
    );
}

#[test]
fn reconciler_task_run_mismatch_assigned_without_run_unassigns_when_required() {
    let now = Utc::now();
    let reconciler = Reconciler::with_config(ReconcilerConfig {
        require_active_worker_runs: true,
        ..Default::default()
    });

    let plan = reconciler.reconcile(input(
        now,
        vec![task(
            "T001",
            TaskStatus::Assigned,
            Some("worker-1"),
            Some("session-1"),
        )],
        vec![worker("worker-1", "session-1", Some("T001"), true)],
        vec![],
    ));

    assert!(matches!(
        plan.task_updates.as_slice(),
        [TaskUpdateAction::UnassignTask {
            task_id,
            reason: TaskRepairReason::MissingActiveRun,
            ..
        }] if task_id.as_str() == "T001"
    ));
}

#[test]
fn reconciler_task_run_mismatch_terminal_task_fails_active_run() {
    let now = Utc::now();
    let active_run = run(
        "run-1",
        "T001",
        RunRole::Worker,
        RunStatus::Running,
        Some("session-1"),
        Some(now + ChronoDuration::minutes(5)),
    );

    let plan = Reconciler::new().reconcile(input(
        now,
        vec![task(
            "T001",
            TaskStatus::Merged,
            Some("worker-1"),
            Some("session-1"),
        )],
        vec![worker("worker-1", "session-1", Some("T001"), true)],
        vec![active_run],
    ));

    assert_eq!(plan.run_failures.len(), 1);
    assert_eq!(
        plan.run_failures[0].reason,
        RunRepairReason::TerminalTaskHasActiveRun
    );
}

#[test]
fn reconciler_task_run_mismatch_role_mismatch_escalates() {
    let now = Utc::now();
    let active_run = run(
        "run-1",
        "T001",
        RunRole::Worker,
        RunStatus::Running,
        Some("session-1"),
        Some(now + ChronoDuration::minutes(5)),
    );

    let plan = Reconciler::new().reconcile(input(
        now,
        vec![task(
            "T001",
            TaskStatus::InReview,
            Some("worker-1"),
            Some("session-1"),
        )],
        vec![worker("worker-1", "session-1", Some("T001"), true)],
        vec![active_run],
    ));

    assert_eq!(plan.escalations.len(), 1);
    assert_eq!(
        plan.escalations[0].reason,
        "run role mismatch for task status"
    );
}

#[test]
fn reconciler_stale_generation_rejection_event_records_fence_values() {
    let now = Utc::now();
    let record = run(
        "run-1",
        "T001",
        RunRole::Worker,
        RunStatus::Running,
        Some("session-1"),
        Some(now + ChronoDuration::minutes(5)),
    );

    let event = Reconciler::stale_run_mutation_rejected_event(
        &record,
        ClaimGeneration::new(0),
        "complete_run",
        now,
    );

    assert!(matches!(
        event.kind,
        EventKind::StaleRunMutationRejected {
            attempted_generation,
            current_generation,
            mutation,
            ..
        } if attempted_generation == ClaimGeneration::new(0)
            && current_generation == ClaimGeneration::new(1)
            && mutation == "complete_run"
    ));
}
