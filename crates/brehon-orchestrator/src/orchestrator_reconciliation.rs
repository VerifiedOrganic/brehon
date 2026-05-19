//! Orchestrator reconciliation application.
//!
//! The pure reconciler produces repair plans. This module is the only place
//! that applies those plans to the live orchestrator projections and ports.

use std::time::Instant;

use chrono::{Duration as ChronoDuration, Utc};
use tracing::{debug, info, warn};

use brehon_ports::{
    ClaimRelease, ClaimRequest, RetryAttemptRequest, RunFailure, RunRetry, RunStore, RunStoreError,
};
use brehon_types::{
    ClaimGeneration, ClaimOwner, Event, EventKind, RunRecord, SessionId, TaskStatus,
};

use crate::error::{OrchestratorError, Result};
use crate::orchestrator::Orchestrator;
use crate::reconciler::{
    ReconciliationEscalation, ReconciliationInput, ReconciliationPlan, RunFailureAction,
    RunReleaseAction, RunRenewalAction, RunRepairReason, RunRetryAttemptAction, TaskUpdateAction,
    WorkerCleanupAction,
};
use crate::retry::{
    decide_retry, RetryDecision, RetryFailureKind, RetryInput, RetryPermissionState,
};
use crate::worker_pool::{WorkerId, WorkerSpawnPlan};

const RECONCILER_OWNER: &str = "brehon-reconciler";
const REPAIR_LEASE_SECONDS: i64 = 60;

#[derive(Debug, Default)]
pub(crate) struct ReconciliationApplyReport {
    pub task_updates: usize,
    pub run_retry_attempts: usize,
    pub run_retries_queued: usize,
    pub run_renewals: usize,
    pub run_releases: usize,
    pub run_failures: usize,
    pub worker_cleanup_actions: usize,
    pub workers_spawned: usize,
    pub escalations: usize,
    pub events_emitted: usize,
}

#[derive(Debug, Default)]
struct ReconciliationActionCounts {
    task_updates: usize,
    run_retry_attempts: usize,
    run_renewals: usize,
    run_releases: usize,
    run_failures: usize,
    worker_cleanup_actions: usize,
    escalations: usize,
}

enum RunFailureApplyOutcome {
    Failed,
    RetryQueued,
}

impl From<&ReconciliationPlan> for ReconciliationActionCounts {
    fn from(plan: &ReconciliationPlan) -> Self {
        Self {
            task_updates: plan.task_updates.len(),
            run_retry_attempts: plan.run_retry_attempts.len(),
            run_renewals: plan.run_renewals.len(),
            run_releases: plan.run_releases.len(),
            run_failures: plan.run_failures.len(),
            worker_cleanup_actions: plan.worker_cleanup_actions.len(),
            escalations: plan.escalations.len(),
        }
    }
}

impl Orchestrator {
    pub async fn run_startup_reconciliation(&mut self) -> Result<()> {
        self.reconcile_before_dispatch("startup").await?;
        Ok(())
    }

    pub(crate) async fn reconcile_before_dispatch(
        &mut self,
        pass: &'static str,
    ) -> Result<ReconciliationApplyReport> {
        let started = Instant::now();
        let now = Utc::now();
        let active_runs = match self.run_store.clone() {
            Some(run_store) => run_store.active_runs().await.map_err(map_run_store_error)?,
            None => Vec::new(),
        };

        let input = {
            let pool = self.worker_pool.read();
            ReconciliationInput::from_current_state(
                now,
                &self.task_board,
                &pool,
                &self.dependency_graph,
                active_runs,
            )
        };
        let plan = self.reconciler.reconcile(input);
        let counts = ReconciliationActionCounts::from(&plan);
        let mut report = ReconciliationApplyReport::default();

        self.apply_task_updates(&plan, &mut report);
        self.apply_worker_cleanup_actions(&plan, &mut report)
            .await?;

        let mut events = Vec::new();
        self.apply_run_actions(&plan, now, &mut events, &mut report)
            .await?;
        self.append_escalation_events(&plan, now, &mut events, &mut report);
        report.events_emitted = self.emit_reconciliation_events(events).await?;

        let duration_ms = started.elapsed().as_millis() as u64;
        info!(
            pass,
            duration_ms,
            planned_task_updates = counts.task_updates,
            planned_run_retry_attempts = counts.run_retry_attempts,
            planned_run_renewals = counts.run_renewals,
            planned_run_releases = counts.run_releases,
            planned_run_failures = counts.run_failures,
            planned_worker_cleanup_actions = counts.worker_cleanup_actions,
            planned_escalations = counts.escalations,
            applied_task_updates = report.task_updates,
            applied_run_retry_attempts = report.run_retry_attempts,
            applied_run_retries_queued = report.run_retries_queued,
            applied_run_renewals = report.run_renewals,
            applied_run_releases = report.run_releases,
            applied_run_failures = report.run_failures,
            applied_worker_cleanup_actions = report.worker_cleanup_actions,
            spawned_workers = report.workers_spawned,
            emitted_events = report.events_emitted,
            "Reconciliation pass complete"
        );

        Ok(report)
    }

    pub(crate) async fn spawn_workers_to_min(&mut self) -> Result<Vec<WorkerId>> {
        let plans = {
            let pool = self.worker_pool.read();
            pool.plan_spawn_to_min()
        };

        if plans.is_empty() {
            debug!("Worker pool already satisfies min_count");
            return Ok(Vec::new());
        }

        let mut spawned = Vec::new();
        for plan in plans {
            if let Some(worker_id) = self.spawn_worker_from_plan(plan).await? {
                spawned.push(worker_id);
            }
        }
        Ok(spawned)
    }

    async fn spawn_worker_from_plan(&mut self, plan: WorkerSpawnPlan) -> Result<Option<WorkerId>> {
        let worker_id = plan.worker_id.clone();
        let session_id = match self.deps.gateway.spawn(plan.spec.clone()).await {
            Ok(session_id) => session_id,
            Err(error) => {
                warn!(worker_id = %worker_id, error = ?error, "Failed to spawn worker");
                return Ok(None);
            }
        };

        let spawned = {
            let mut pool = self.worker_pool.write();
            pool.record_spawned_worker(plan, session_id)
        };
        Ok(Some(spawned))
    }

    fn apply_task_updates(
        &mut self,
        plan: &ReconciliationPlan,
        report: &mut ReconciliationApplyReport,
    ) {
        for action in &plan.task_updates {
            match action {
                TaskUpdateAction::UnassignTask {
                    task_id, reason, ..
                } => match self.task_board.unassign_task(task_id) {
                    Ok(()) => {
                        report.task_updates += 1;
                        debug!(task_id = %task_id, reason = ?reason, "Applied reconciliation task update");
                    }
                    Err(error) => {
                        warn!(task_id = %task_id, reason = ?reason, error = ?error, "Failed to apply reconciliation task update");
                    }
                },
            }
        }
    }

    async fn apply_worker_cleanup_actions(
        &mut self,
        plan: &ReconciliationPlan,
        report: &mut ReconciliationApplyReport,
    ) -> Result<()> {
        for action in &plan.worker_cleanup_actions {
            match action {
                WorkerCleanupAction::ClearAssignment { worker_id, reason } => {
                    let result = {
                        let mut pool = self.worker_pool.write();
                        pool.clear_assignment(worker_id)
                    };
                    match result {
                        Ok(()) => {
                            report.worker_cleanup_actions += 1;
                            debug!(worker_id = %worker_id, reason = ?reason, "Applied reconciliation worker cleanup");
                        }
                        Err(error) => {
                            warn!(worker_id = %worker_id, reason = ?reason, error = ?error, "Failed to clear worker assignment during reconciliation");
                        }
                    }
                }
                WorkerCleanupAction::HandleWorkerDeath { session_id } => {
                    let spawn_plan = {
                        let mut pool = self.worker_pool.write();
                        pool.reconcile_worker_death(session_id)
                    };
                    match spawn_plan {
                        Ok(Some(spawn_plan)) => {
                            report.worker_cleanup_actions += 1;
                            if self.spawn_worker_from_plan(spawn_plan).await?.is_some() {
                                report.workers_spawned += 1;
                            }
                        }
                        Ok(None) => {
                            report.worker_cleanup_actions += 1;
                        }
                        Err(error) => {
                            warn!(session_id = %session_id, error = ?error, "Failed to process worker death during reconciliation");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn apply_run_actions(
        &self,
        plan: &ReconciliationPlan,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
        report: &mut ReconciliationApplyReport,
    ) -> Result<()> {
        let Some(run_store) = self.run_store.clone() else {
            return Ok(());
        };
        let run_store = run_store.as_ref();

        for action in &plan.run_retry_attempts {
            if self
                .apply_run_retry_attempt(run_store, action, now, events)
                .await?
            {
                report.run_retry_attempts += 1;
            }
        }
        for action in &plan.run_renewals {
            if self
                .apply_run_renewal(run_store, action, now, events)
                .await?
            {
                report.run_renewals += 1;
            }
        }
        for action in &plan.run_releases {
            if self
                .apply_run_release(run_store, action, now, events)
                .await?
            {
                report.run_releases += 1;
            }
        }
        for action in &plan.run_failures {
            match self
                .apply_run_failure(run_store, action, now, events)
                .await?
            {
                Some(RunFailureApplyOutcome::Failed) => report.run_failures += 1,
                Some(RunFailureApplyOutcome::RetryQueued) => report.run_retries_queued += 1,
                None => {}
            }
        }

        Ok(())
    }

    async fn apply_run_renewal(
        &self,
        run_store: &dyn RunStore,
        action: &RunRenewalAction,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> Result<bool> {
        let mut record = action.run.clone();
        if record.claim_is_expired_at(now) {
            let owner = record.claim_owner.clone().unwrap_or_else(reconciler_owner);
            record = self
                .claim_run_for_repair(
                    run_store,
                    &record,
                    owner,
                    record.session_id.clone(),
                    now,
                    events,
                )
                .await?;
        }

        match run_store
            .renew_claim(&record.run_id, record.claim_generation)
            .await
        {
            Ok(renewed) => {
                events.push(run_claim_renewed_event(&renewed, now));
                Ok(true)
            }
            Err(RunStoreError::LeaseExpired { .. }) => {
                let owner = record.claim_owner.clone().unwrap_or_else(reconciler_owner);
                let claimed = self
                    .claim_run_for_repair(
                        run_store,
                        &record,
                        owner,
                        record.session_id.clone(),
                        now,
                        events,
                    )
                    .await?;
                let renewed = run_store
                    .renew_claim(&claimed.run_id, claimed.claim_generation)
                    .await
                    .map_err(map_run_store_error)?;
                events.push(run_claim_renewed_event(&renewed, now));
                Ok(true)
            }
            Err(error) => {
                self.handle_run_store_error(
                    run_store,
                    error,
                    record.claim_generation,
                    "renew_claim",
                    now,
                    events,
                )
                .await
            }
        }
    }

    async fn apply_run_retry_attempt(
        &self,
        run_store: &dyn RunStore,
        action: &RunRetryAttemptAction,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> Result<bool> {
        let request = RetryAttemptRequest::new(
            action.run.run_id.clone(),
            action.run.claim_generation,
            retry_attempt_run_id(&action.run),
            now,
        );

        match run_store.start_retry_attempt(request).await {
            Ok(started) => {
                events.push(run_failed_event(
                    &started.queued_run,
                    format!("retry attempt started: {}", action.reason.as_str()),
                    now,
                ));
                events.push(run_created_event(&started.retry_run, now));
                Ok(true)
            }
            Err(error) => {
                self.handle_run_store_error(
                    run_store,
                    error,
                    action.run.claim_generation,
                    "start_retry_attempt",
                    now,
                    events,
                )
                .await
            }
        }
    }

    async fn apply_run_release(
        &self,
        run_store: &dyn RunStore,
        action: &RunReleaseAction,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> Result<bool> {
        let mut record = action.run.clone();
        if record.claim_is_expired_at(now) {
            record = self
                .claim_run_for_repair(run_store, &record, reconciler_owner(), None, now, events)
                .await?;
        }

        let reason = Some(action.reason.as_str().to_string());
        match run_store
            .release_claim(ClaimRelease::new(
                record.run_id.clone(),
                record.claim_generation,
                now,
                reason.clone(),
            ))
            .await
        {
            Ok(released) => {
                events.push(run_released_event(&released, reason, now));
                Ok(true)
            }
            Err(RunStoreError::LeaseExpired { .. }) => {
                let claimed = self
                    .claim_run_for_repair(run_store, &record, reconciler_owner(), None, now, events)
                    .await?;
                let released = run_store
                    .release_claim(ClaimRelease::new(
                        claimed.run_id.clone(),
                        claimed.claim_generation,
                        now,
                        reason.clone(),
                    ))
                    .await
                    .map_err(map_run_store_error)?;
                events.push(run_released_event(&released, reason, now));
                Ok(true)
            }
            Err(error) => {
                self.handle_run_store_error(
                    run_store,
                    error,
                    record.claim_generation,
                    "release_claim",
                    now,
                    events,
                )
                .await
            }
        }
    }

    async fn apply_run_failure(
        &self,
        run_store: &dyn RunStore,
        action: &RunFailureAction,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> Result<Option<RunFailureApplyOutcome>> {
        match self.retry_decision_for_failure(action) {
            RetryDecision::RetryNow {
                next_attempt: _,
                reason,
            } => {
                return self
                    .queue_run_retry(run_store, action, now, now, reason, events)
                    .await;
            }
            RetryDecision::RetryLater {
                next_attempt: _,
                delay_ms,
                jitter_ms: _,
                reason,
            } => {
                let retry_at = now + ChronoDuration::milliseconds(delay_ms as i64);
                return self
                    .queue_run_retry(run_store, action, now, retry_at, reason, events)
                    .await;
            }
            RetryDecision::Escalate { reason } => {
                events.push(retry_escalation_event(action, reason, now));
            }
            RetryDecision::FailTerminal { .. } | RetryDecision::NoAction { .. } => {}
        }

        let mut record = action.run.clone();
        if !record.status.has_active_claim() || record.claim_is_expired_at(now) {
            record = self
                .claim_run_for_repair(run_store, &record, reconciler_owner(), None, now, events)
                .await?;
        }

        let reason = action.reason.as_str().to_string();
        match run_store
            .fail_run(RunFailure::new(
                record.run_id.clone(),
                record.claim_generation,
                now,
                reason.clone(),
            ))
            .await
        {
            Ok(failed) => {
                events.push(run_failed_event(&failed, reason, now));
                Ok(Some(RunFailureApplyOutcome::Failed))
            }
            Err(RunStoreError::LeaseExpired { .. }) => {
                let claimed = self
                    .claim_run_for_repair(run_store, &record, reconciler_owner(), None, now, events)
                    .await?;
                let failed = run_store
                    .fail_run(RunFailure::new(
                        claimed.run_id.clone(),
                        claimed.claim_generation,
                        now,
                        reason.clone(),
                    ))
                    .await
                    .map_err(map_run_store_error)?;
                events.push(run_failed_event(&failed, reason, now));
                Ok(Some(RunFailureApplyOutcome::Failed))
            }
            Err(error) => self
                .handle_run_store_error(
                    run_store,
                    error,
                    record.claim_generation,
                    "fail_run",
                    now,
                    events,
                )
                .await
                .map(|applied| applied.then_some(RunFailureApplyOutcome::Failed)),
        }
    }

    async fn queue_run_retry(
        &self,
        run_store: &dyn RunStore,
        action: &RunFailureAction,
        queued_at: chrono::DateTime<Utc>,
        retry_at: chrono::DateTime<Utc>,
        reason: &'static str,
        events: &mut Vec<Event>,
    ) -> Result<Option<RunFailureApplyOutcome>> {
        let mut record = action.run.clone();
        if !record.status.has_active_claim() || record.claim_is_expired_at(queued_at) {
            record = self
                .claim_run_for_repair(
                    run_store,
                    &record,
                    reconciler_owner(),
                    None,
                    queued_at,
                    events,
                )
                .await?;
        }

        let retry_reason = format!("{}: {}", action.reason.as_str(), reason);
        match run_store
            .queue_retry(RunRetry::new(
                record.run_id.clone(),
                record.claim_generation,
                queued_at,
                retry_at,
                retry_reason.clone(),
            ))
            .await
        {
            Ok(queued) => {
                events.push(run_retry_queued_event(&queued, retry_reason, queued_at));
                Ok(Some(RunFailureApplyOutcome::RetryQueued))
            }
            Err(RunStoreError::LeaseExpired { .. }) => {
                let claimed = self
                    .claim_run_for_repair(
                        run_store,
                        &record,
                        reconciler_owner(),
                        None,
                        queued_at,
                        events,
                    )
                    .await?;
                let queued = run_store
                    .queue_retry(RunRetry::new(
                        claimed.run_id.clone(),
                        claimed.claim_generation,
                        queued_at,
                        retry_at,
                        retry_reason.clone(),
                    ))
                    .await
                    .map_err(map_run_store_error)?;
                events.push(run_retry_queued_event(&queued, retry_reason, queued_at));
                Ok(Some(RunFailureApplyOutcome::RetryQueued))
            }
            Err(error) => self
                .handle_run_store_error(
                    run_store,
                    error,
                    record.claim_generation,
                    "queue_retry",
                    queued_at,
                    events,
                )
                .await
                .map(|applied| applied.then_some(RunFailureApplyOutcome::RetryQueued)),
        }
    }

    fn retry_decision_for_failure(&self, action: &RunFailureAction) -> RetryDecision {
        let task_status = self
            .task_board
            .get_task(&action.run.task_id)
            .map(|task| task.status)
            .unwrap_or(TaskStatus::Pending);
        let failure_kind = match action.reason {
            RunRepairReason::ExpiredRunningClaim | RunRepairReason::DeadOrMissingSession => {
                RetryFailureKind::Interrupted
            }
            RunRepairReason::ExpiredClaim
            | RunRepairReason::ExpiredClaimWithLiveOperation
            | RunRepairReason::TerminalTaskHasActiveRun
            | RunRepairReason::RetryDue => RetryFailureKind::Deterministic,
        };

        decide_retry(
            RetryInput {
                run_status: action.run.status,
                failure_kind,
                attempt_count: action.run.attempt,
                task_status,
                review_status: None,
                permission_state: Some(RetryPermissionState::NotRequired),
                operator_override: None,
            },
            self.config.retry_policy,
        )
    }

    async fn claim_run_for_repair(
        &self,
        run_store: &dyn RunStore,
        run: &RunRecord,
        owner: ClaimOwner,
        session_id: Option<SessionId>,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> Result<RunRecord> {
        let request = ClaimRequest::new(
            run.run_id.clone(),
            owner,
            session_id,
            now,
            now + ChronoDuration::seconds(REPAIR_LEASE_SECONDS),
        );
        let claimed = run_store
            .claim_run(request)
            .await
            .map_err(map_run_store_error)?;
        events.push(run_claimed_event(&claimed, now));
        Ok(claimed)
    }

    async fn handle_run_store_error(
        &self,
        run_store: &dyn RunStore,
        error: RunStoreError,
        attempted_generation: ClaimGeneration,
        mutation: &'static str,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> Result<bool> {
        match error {
            RunStoreError::StaleGeneration { run_id, actual, .. } => {
                let attempted = if actual == attempted_generation {
                    actual
                } else {
                    attempted_generation
                };
                if let Some(record) = run_store
                    .get_run(&run_id)
                    .await
                    .map_err(map_run_store_error)?
                {
                    events.push(crate::Reconciler::stale_run_mutation_rejected_event(
                        &record, attempted, mutation, now,
                    ));
                }
                Ok(false)
            }
            other => Err(map_run_store_error(other)),
        }
    }

    fn append_escalation_events(
        &self,
        plan: &ReconciliationPlan,
        now: chrono::DateTime<Utc>,
        events: &mut Vec<Event>,
        report: &mut ReconciliationApplyReport,
    ) {
        for escalation in &plan.escalations {
            events.push(escalation_event(escalation, now));
            report.escalations += 1;
        }
    }

    async fn emit_reconciliation_events(&self, events: Vec<Event>) -> Result<usize> {
        let mut emitted = 0;
        for event in events {
            self.deps.event_store.append(event).await?;
            emitted += 1;
        }
        Ok(emitted)
    }
}

fn reconciler_owner() -> ClaimOwner {
    ClaimOwner::new(RECONCILER_OWNER)
}

fn run_claimed_event(record: &RunRecord, now: chrono::DateTime<Utc>) -> Event {
    Event {
        aggregate_id: record.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::RunClaimed {
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            owner: record.claim_owner.clone().unwrap_or_else(reconciler_owner),
            session_id: record.session_id.clone(),
            generation: record.claim_generation,
            lease_expires_at: record.lease_expires_at.unwrap_or(now),
        },
    }
}

fn run_claim_renewed_event(record: &RunRecord, now: chrono::DateTime<Utc>) -> Event {
    Event {
        aggregate_id: record.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::RunClaimRenewed {
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            owner: record.claim_owner.clone().unwrap_or_else(reconciler_owner),
            generation: record.claim_generation,
            lease_expires_at: record.lease_expires_at.unwrap_or(now),
        },
    }
}

fn run_created_event(record: &RunRecord, now: chrono::DateTime<Utc>) -> Event {
    Event {
        aggregate_id: record.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::RunCreated {
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            status: record.status,
        },
    }
}

fn run_released_event(
    record: &RunRecord,
    reason: Option<String>,
    now: chrono::DateTime<Utc>,
) -> Event {
    Event {
        aggregate_id: record.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::RunReleased {
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            generation: record.claim_generation,
            reason,
            released_at: now,
        },
    }
}

fn run_failed_event(record: &RunRecord, reason: String, now: chrono::DateTime<Utc>) -> Event {
    Event {
        aggregate_id: record.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::RunFailed {
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            generation: record.claim_generation,
            reason,
            failed_at: now,
        },
    }
}

fn run_retry_queued_event(record: &RunRecord, reason: String, now: chrono::DateTime<Utc>) -> Event {
    Event {
        aggregate_id: record.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::RunRetryQueued {
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            generation: record.claim_generation,
            reason,
            queued_at: record.retry_queued_at.unwrap_or(now),
            retry_at: record.retry_at,
        },
    }
}

fn retry_escalation_event(
    action: &RunFailureAction,
    reason: &'static str,
    now: chrono::DateTime<Utc>,
) -> Event {
    Event {
        aggregate_id: action.run.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::EscalationTriggered {
            reason: format!("retry decision escalated: {reason}"),
            context: format!(
                "run {} task {} role {} attempt {} status {}",
                action.run.run_id,
                action.run.task_id,
                action.run.role,
                action.run.attempt,
                action.run.status
            ),
        },
    }
}

fn retry_attempt_run_id(record: &RunRecord) -> brehon_types::RunId {
    brehon_types::RunId::new(format!(
        "{}-retry-{}",
        record.run_id.as_str(),
        record.attempt.saturating_add(1)
    ))
}

fn escalation_event(escalation: &ReconciliationEscalation, now: chrono::DateTime<Utc>) -> Event {
    Event {
        aggregate_id: escalation
            .task_id
            .as_ref()
            .map(|id| id.as_str())
            .or_else(|| escalation.run_id.as_ref().map(|id| id.as_str()))
            .unwrap_or("reconciler")
            .to_string(),
        timestamp: now,
        kind: EventKind::EscalationTriggered {
            reason: escalation.reason.clone(),
            context: escalation.context.clone(),
        },
    }
}

fn map_run_store_error(err: RunStoreError) -> OrchestratorError {
    match err {
        RunStoreError::Storage(message) | RunStoreError::Serialization(message) => {
            OrchestratorError::StorageError(message)
        }
        other => OrchestratorError::PortError(other.to_string()),
    }
}
