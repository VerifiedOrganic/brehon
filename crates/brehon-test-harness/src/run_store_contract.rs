//! Reusable contract tests for durable run store implementations.

use std::time::Duration;

use brehon_ports::{
    ClaimRelease, ClaimRequest, RetryAttemptRequest, RunCompletion, RunContinuation, RunRetry,
    RunStore, RunStoreError, RunStoreResult,
};
use brehon_types::{ClaimOwner, RunId, RunRecord, RunRole, RunStatus, SessionId, TaskId};
use chrono::Utc;

fn run_record(run_id: &str, task_id: &str, role: RunRole) -> RunRecord {
    RunRecord::new(RunId::new(run_id), TaskId::new(task_id), role, Utc::now())
}

fn claim_request(run_id: &str, owner: &str, lease_for: Duration) -> ClaimRequest {
    let now = Utc::now();
    ClaimRequest::new(
        RunId::new(run_id),
        ClaimOwner::new(owner),
        Some(SessionId::new(format!("session-{owner}"))),
        now,
        now + chrono::Duration::from_std(lease_for).unwrap(),
    )
}

async fn create_worker_run<S: RunStore + ?Sized>(
    store: &S,
    run_id: &str,
    task_id: &str,
) -> RunStoreResult<RunRecord> {
    store
        .create_run(run_record(run_id, task_id, RunRole::Worker))
        .await
}

/// Contract: a created non-terminal run is queryable and active.
pub async fn create_active_run<S: RunStore + ?Sized>(store: &S) {
    let created = create_worker_run(store, "run-create", "T-create")
        .await
        .expect("create run should succeed");

    assert_eq!(created.status, RunStatus::Created);
    assert_eq!(created.claim_generation.as_u64(), 0);

    let fetched = store
        .get_run(&created.run_id)
        .await
        .expect("get run should succeed")
        .expect("run should exist");
    assert_eq!(fetched, created);

    let active = store.active_runs().await.expect("active runs should load");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].run_id, created.run_id);

    let by_task = store
        .runs_for_task(&TaskId::new("T-create"))
        .await
        .expect("task runs should load");
    assert_eq!(by_task.len(), 1);
    assert_eq!(by_task[0].run_id, created.run_id);
}

/// Contract: a second owner cannot claim a live claimed run.
pub async fn reject_duplicate_claim<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-dup-claim", "T-dup-claim")
        .await
        .expect("create run should succeed");

    let first = store
        .claim_run(claim_request(
            "run-dup-claim",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("first claim should succeed");
    assert_eq!(first.claim_generation.as_u64(), 1);

    let second = store
        .claim_run(claim_request(
            "run-dup-claim",
            "worker-2",
            Duration::from_secs(60),
        ))
        .await;

    assert!(matches!(
        second,
        Err(RunStoreError::DuplicateActiveClaim { .. })
    ));
}

/// Contract: a second active run for the same task/role is rejected.
pub async fn reject_duplicate_active_run<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-active-a", "T-active-dup")
        .await
        .expect("first active run should be created");

    let duplicate = create_worker_run(store, "run-active-b", "T-active-dup").await;

    assert!(matches!(
        duplicate,
        Err(RunStoreError::DuplicateActiveClaim { .. })
    ));
}

/// Contract: renewing a live claim keeps the generation and extends the lease.
pub async fn renew_active_claim<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-renew", "T-renew")
        .await
        .expect("create run should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-renew",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");
    let previous_expiry = claimed
        .lease_expires_at
        .expect("claimed run should have a lease");

    let renewed = store
        .renew_claim(&claimed.run_id, claimed.claim_generation)
        .await
        .expect("renew should succeed");

    assert_eq!(renewed.claim_generation, claimed.claim_generation);
    assert!(renewed.lease_expires_at.unwrap() >= previous_expiry);
}

/// Contract: an expired claim cannot be renewed.
pub async fn reject_expired_claim_renewal<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-expired-renew", "T-expired-renew")
        .await
        .expect("create run should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-expired-renew",
            "worker-1",
            Duration::from_millis(5),
        ))
        .await
        .expect("claim should succeed");

    std::thread::sleep(Duration::from_millis(15));

    let renewed = store
        .renew_claim(&claimed.run_id, claimed.claim_generation)
        .await;
    assert!(matches!(renewed, Err(RunStoreError::LeaseExpired { .. })));
}

/// Contract: releasing a claim makes the same run claimable again.
pub async fn release_and_requeue<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-release", "T-release")
        .await
        .expect("create run should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-release",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");

    let released = store
        .release_claim(ClaimRelease::new(
            claimed.run_id.clone(),
            claimed.claim_generation,
            Utc::now(),
            Some("worker unavailable".into()),
        ))
        .await
        .expect("release should succeed");

    assert_eq!(released.status, RunStatus::Released);
    assert!(released.claim_owner.is_none());

    let reclaimed = store
        .claim_run(claim_request(
            "run-release",
            "worker-2",
            Duration::from_secs(60),
        ))
        .await
        .expect("reclaim after release should succeed");

    assert_eq!(
        reclaimed.claim_generation.as_u64(),
        claimed.claim_generation.as_u64() + 1
    );
    assert_eq!(
        reclaimed.claim_owner.as_ref().map(ClaimOwner::as_str),
        Some("worker-2")
    );
}

/// Contract: reclaiming an expired claim increments generation.
pub async fn reclaim_increments_generation<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-reclaim", "T-reclaim")
        .await
        .expect("create run should succeed");
    let first = store
        .claim_run(claim_request(
            "run-reclaim",
            "worker-1",
            Duration::from_millis(5),
        ))
        .await
        .expect("first claim should succeed");

    std::thread::sleep(Duration::from_millis(15));

    let second = store
        .claim_run(claim_request(
            "run-reclaim",
            "worker-2",
            Duration::from_secs(60),
        ))
        .await
        .expect("expired claim should be reclaimable");

    assert_eq!(
        second.claim_generation.as_u64(),
        first.claim_generation.as_u64() + 1
    );
    assert_eq!(
        second.claim_owner.as_ref().map(ClaimOwner::as_str),
        Some("worker-2")
    );
}

/// Contract: stale generation completion is rejected.
pub async fn stale_completion_rejected<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-stale-complete", "T-stale-complete")
        .await
        .expect("create run should succeed");
    let first = store
        .claim_run(claim_request(
            "run-stale-complete",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");

    store
        .release_claim(ClaimRelease::new(
            first.run_id.clone(),
            first.claim_generation,
            Utc::now(),
            Some("release for stale test".into()),
        ))
        .await
        .expect("release should succeed");

    let second = store
        .claim_run(claim_request(
            "run-stale-complete",
            "worker-2",
            Duration::from_secs(60),
        ))
        .await
        .expect("reclaim should succeed");
    assert_ne!(first.claim_generation, second.claim_generation);

    let stale = store
        .complete_run(RunCompletion::new(
            first.run_id.clone(),
            first.claim_generation,
            Utc::now(),
            Some("stale completion".into()),
        ))
        .await;

    assert!(matches!(stale, Err(RunStoreError::StaleGeneration { .. })));
}

/// Contract: continuation records bounded same-run progress without creating a new run.
pub async fn continuation_records_same_run_progress<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-continuation", "T-continuation")
        .await
        .expect("create continuation candidate should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-continuation",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");

    let continued_at = Utc::now();
    let continued = store
        .record_continuation(RunContinuation::new(
            claimed.run_id.clone(),
            claimed.claim_generation,
            continued_at,
            1,
        ))
        .await
        .expect("continuation should record on active claim");

    assert_eq!(continued.run_id, claimed.run_id);
    assert_eq!(continued.claim_generation, claimed.claim_generation);
    assert_eq!(continued.continuation_turns, 1);
    assert_eq!(continued.last_continuation_at, Some(continued_at));
    assert_eq!(continued.last_activity_at, Some(continued_at));

    let stale_turn = store
        .record_continuation(RunContinuation::new(
            claimed.run_id.clone(),
            claimed.claim_generation,
            Utc::now(),
            1,
        ))
        .await;
    assert!(matches!(
        stale_turn,
        Err(RunStoreError::InvalidStatusTransition { .. })
    ));

    let by_task = store
        .runs_for_task(&TaskId::new("T-continuation"))
        .await
        .expect("task runs should load");
    assert_eq!(by_task.len(), 1);
    assert_eq!(by_task[0].run_id, claimed.run_id);
}

/// Contract: terminal runs are excluded from active run queries.
pub async fn active_run_query_excludes_terminal_runs<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-terminal", "T-terminal")
        .await
        .expect("create terminal candidate should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-terminal",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");

    store
        .complete_run(RunCompletion::new(
            claimed.run_id.clone(),
            claimed.claim_generation,
            Utc::now(),
            Some("done".into()),
        ))
        .await
        .expect("complete should succeed");

    create_worker_run(store, "run-still-active", "T-still-active")
        .await
        .expect("second active run should succeed");

    let active = store.active_runs().await.expect("active runs should load");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].run_id, RunId::new("run-still-active"));
}

/// Contract: queueing a retry stores due-time metadata and blocks early claim.
pub async fn retry_queued_blocks_claim_until_due<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-retry-wait", "T-retry-wait")
        .await
        .expect("create retry candidate should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-retry-wait",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");

    let now = Utc::now();
    let retry_at = now + chrono::Duration::seconds(60);
    let queued = store
        .queue_retry(RunRetry::new(
            claimed.run_id.clone(),
            claimed.claim_generation,
            now,
            retry_at,
            "worker interrupted",
        ))
        .await
        .expect("queue retry should succeed");

    assert_eq!(queued.status, RunStatus::RetryQueued);
    assert_eq!(queued.retry_at, Some(retry_at));
    assert_eq!(queued.retry_queued_at, Some(now));
    assert_eq!(queued.retry_reason.as_deref(), Some("worker interrupted"));
    assert!(queued.claim_owner.is_none());

    let early_claim = store
        .claim_run(claim_request(
            "run-retry-wait",
            "worker-2",
            Duration::from_secs(60),
        ))
        .await;
    assert!(matches!(
        early_claim,
        Err(RunStoreError::RetryNotDue { .. })
    ));
}

/// Contract: a due retry closes the queued run and creates a new attempt.
pub async fn retry_attempt_creates_new_run<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-retry-due", "T-retry-due")
        .await
        .expect("create retry candidate should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-retry-due",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");

    let now = Utc::now();
    let queued = store
        .queue_retry(RunRetry::new(
            claimed.run_id.clone(),
            claimed.claim_generation,
            now,
            now,
            "retry now",
        ))
        .await
        .expect("queue retry should succeed");

    let started = store
        .start_retry_attempt(RetryAttemptRequest::new(
            queued.run_id.clone(),
            queued.claim_generation,
            RunId::new("run-retry-due-attempt-2"),
            now,
        ))
        .await
        .expect("due retry should create next attempt");

    assert_eq!(started.queued_run.status, RunStatus::Failed);
    assert_eq!(started.retry_run.status, RunStatus::Created);
    assert_eq!(started.retry_run.task_id, TaskId::new("T-retry-due"));
    assert_eq!(started.retry_run.role, RunRole::Worker);
    assert_eq!(started.retry_run.attempt, queued.attempt + 1);

    let active = store.active_runs().await.expect("active runs should load");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].run_id, RunId::new("run-retry-due-attempt-2"));
}

/// Contract: retry queueing is a one-way transition for an attempt.
pub async fn duplicate_retry_queue_rejected<S: RunStore + ?Sized>(store: &S) {
    create_worker_run(store, "run-retry-dup", "T-retry-dup")
        .await
        .expect("create retry candidate should succeed");
    let claimed = store
        .claim_run(claim_request(
            "run-retry-dup",
            "worker-1",
            Duration::from_secs(60),
        ))
        .await
        .expect("claim should succeed");

    let now = Utc::now();
    store
        .queue_retry(RunRetry::new(
            claimed.run_id.clone(),
            claimed.claim_generation,
            now,
            now,
            "first retry",
        ))
        .await
        .expect("first retry queue should succeed");

    let duplicate = store
        .queue_retry(RunRetry::new(
            claimed.run_id,
            claimed.claim_generation,
            now,
            now,
            "duplicate retry",
        ))
        .await;

    assert!(matches!(
        duplicate,
        Err(RunStoreError::InvalidStatusTransition { .. })
    ));
}
