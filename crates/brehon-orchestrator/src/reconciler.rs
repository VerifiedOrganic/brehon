//! Snapshot-based reconciliation planning for orchestrator state.
//!
//! The reconciler intentionally plans repairs from immutable snapshots. Phase 3A
//! keeps it out of the live tick so the repair policy can be tested before it is
//! allowed to change dispatch behavior.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use brehon_ports::{EventStore, RunStore, RunStoreError};
use brehon_types::{
    ClaimGeneration, Event, EventKind, RunId, RunRecord, RunRole, RunStatus, SessionId, TaskId,
    TaskStatus,
};

use crate::dependency_graph::DependencyGraph;
use crate::error::{OrchestratorError, Result};
use crate::task_board::{TaskBoard, TaskEntry};
use crate::task_lifecycle::TaskLifecycle;
use crate::worker_pool::{WorkerId, WorkerInfo, WorkerKind, WorkerPool};

/// Reconciler policy knobs that should stay explicit at call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcilerConfig {
    /// When true, assigned worker-owned tasks without an active worker run are
    /// planned for unassignment. Default is false while run state is not wired
    /// into dispatch.
    pub require_active_worker_runs: bool,
    /// When true, expired claims held by alive sessions with matching active
    /// work are planned for renewal instead of failure/release.
    pub renew_live_expired_claims: bool,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            require_active_worker_runs: false,
            renew_live_expired_claims: true,
        }
    }
}

/// Stateless reconciler.
#[derive(Debug, Clone, Default)]
pub struct Reconciler {
    config: ReconcilerConfig,
}

impl Reconciler {
    /// Create a reconciler with default policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a reconciler with explicit policy.
    pub fn with_config(config: ReconcilerConfig) -> Self {
        Self { config }
    }

    /// Build a reconciliation plan from a complete snapshot.
    pub fn reconcile(&self, input: ReconciliationInput) -> ReconciliationPlan {
        let mut plan = ReconciliationPlan::default();
        self.reconcile_retry_queued_runs(&input, &mut plan);
        self.reconcile_dead_workers(&input, &mut plan);
        self.reconcile_claims(&input, &mut plan);
        self.reconcile_task_run_mismatches(&input, &mut plan);
        plan
    }

    /// Load active runs through the durable run-store port and plan repairs
    /// from the supplied task/worker/dependency snapshot.
    pub async fn reconcile_with_run_store(
        &self,
        run_store: &dyn RunStore,
        mut input: ReconciliationInput,
    ) -> Result<ReconciliationPlan> {
        input.active_runs = run_store.active_runs().await.map_err(map_run_store_error)?;
        Ok(self.reconcile(input))
    }

    /// Append already-built reconciliation events. This keeps event persistence
    /// explicit instead of hiding writes inside the pure planner.
    pub async fn emit_events(
        &self,
        event_store: &dyn EventStore,
        events: Vec<Event>,
    ) -> Result<Vec<brehon_types::EventId>> {
        let mut ids = Vec::with_capacity(events.len());
        for event in events {
            ids.push(event_store.append(event).await?);
        }
        Ok(ids)
    }

    /// Convert planned run actions and escalations into durable event payloads.
    /// Callers should emit these only after applying the corresponding repair.
    pub fn events_for_plan(&self, plan: &ReconciliationPlan, now: DateTime<Utc>) -> Vec<Event> {
        let mut events = Vec::new();

        for action in &plan.run_renewals {
            if let Some(owner) = action.run.claim_owner.clone() {
                events.push(Event {
                    aggregate_id: action.run.task_id.as_str().to_string(),
                    timestamp: now,
                    kind: EventKind::RunClaimRenewed {
                        run_id: action.run.run_id.clone(),
                        task_id: action.run.task_id.clone(),
                        role: action.run.role.clone(),
                        owner,
                        generation: action.run.claim_generation,
                        lease_expires_at: action.run.lease_expires_at.unwrap_or(action.observed_at),
                    },
                });
            }
        }

        for action in &plan.run_releases {
            events.push(Event {
                aggregate_id: action.run.task_id.as_str().to_string(),
                timestamp: now,
                kind: EventKind::RunReleased {
                    run_id: action.run.run_id.clone(),
                    task_id: action.run.task_id.clone(),
                    role: action.run.role.clone(),
                    generation: action.run.claim_generation,
                    reason: Some(action.reason.as_str().to_string()),
                    released_at: action.observed_at,
                },
            });
        }

        for action in &plan.run_failures {
            events.push(Event {
                aggregate_id: action.run.task_id.as_str().to_string(),
                timestamp: now,
                kind: EventKind::RunFailed {
                    run_id: action.run.run_id.clone(),
                    task_id: action.run.task_id.clone(),
                    role: action.run.role.clone(),
                    generation: action.run.claim_generation,
                    reason: action.reason.as_str().to_string(),
                    failed_at: action.observed_at,
                },
            });
        }

        for escalation in &plan.escalations {
            events.push(Event {
                aggregate_id: escalation
                    .task_id
                    .as_ref()
                    .map(TaskId::as_str)
                    .or_else(|| escalation.run_id.as_ref().map(RunId::as_str))
                    .unwrap_or("reconciler")
                    .to_string(),
                timestamp: now,
                kind: EventKind::EscalationTriggered {
                    reason: escalation.reason.clone(),
                    context: escalation.context.clone(),
                },
            });
        }

        events
    }

    /// Build the event that records a stale run mutation rejected by a durable
    /// claim-generation fence.
    pub fn stale_run_mutation_rejected_event(
        record: &RunRecord,
        attempted_generation: ClaimGeneration,
        mutation: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Event {
        Event {
            aggregate_id: record.task_id.as_str().to_string(),
            timestamp: now,
            kind: EventKind::StaleRunMutationRejected {
                run_id: record.run_id.clone(),
                task_id: record.task_id.clone(),
                role: record.role.clone(),
                attempted_generation,
                current_generation: record.claim_generation,
                mutation: mutation.into(),
            },
        }
    }

    fn reconcile_retry_queued_runs(
        &self,
        input: &ReconciliationInput,
        plan: &mut ReconciliationPlan,
    ) {
        for run in input
            .active_runs
            .iter()
            .filter(|run| run.status == RunStatus::RetryQueued)
        {
            if run.retry_is_due_at(input.now) {
                plan.add_run_retry_attempt(RunRetryAttemptAction {
                    run: run.clone(),
                    observed_at: input.now,
                    reason: RunRepairReason::RetryDue,
                });
            }
        }
    }

    fn reconcile_dead_workers(&self, input: &ReconciliationInput, plan: &mut ReconciliationPlan) {
        let workers_by_id: HashMap<WorkerId, &WorkerSnapshotEntry> = input
            .workers
            .iter()
            .map(|worker| (worker.worker_id.clone(), worker))
            .collect();
        let tasks_by_id: HashMap<TaskId, &TaskEntry> = input
            .tasks
            .iter()
            .map(|task| (task.id.clone(), task))
            .collect();

        for worker in input.workers.iter().filter(|worker| !worker.is_alive) {
            plan.add_worker_cleanup(WorkerCleanupAction::HandleWorkerDeath {
                session_id: worker.session_id.clone(),
            });
        }

        for task in &input.tasks {
            if !is_worker_owned_status(task.status) {
                continue;
            }

            let Some(assignee) = task.assignee.as_ref() else {
                continue;
            };
            let worker_id = WorkerId::new(assignee.clone());

            match workers_by_id.get(&worker_id).copied() {
                None => {
                    plan.add_task_update(TaskUpdateAction::UnassignTask {
                        task_id: task.id.clone(),
                        worker_id: None,
                        reason: TaskRepairReason::MissingWorker,
                    });
                }
                Some(worker) if !worker.is_alive => {
                    plan.add_task_update(TaskUpdateAction::UnassignTask {
                        task_id: task.id.clone(),
                        worker_id: Some(worker_id),
                        reason: TaskRepairReason::DeadWorker,
                    });
                }
                Some(worker) if task_session_is_stale(task, worker) => {
                    plan.add_task_update(TaskUpdateAction::UnassignTask {
                        task_id: task.id.clone(),
                        worker_id: Some(worker_id.clone()),
                        reason: TaskRepairReason::StaleSession,
                    });
                    plan.add_worker_cleanup(WorkerCleanupAction::ClearAssignment {
                        worker_id,
                        reason: WorkerCleanupReason::TaskSessionMismatch,
                    });
                }
                _ => {}
            }
        }

        for worker in &input.workers {
            let Some(assigned_task) = worker.assigned_task.as_ref() else {
                continue;
            };
            let clear_reason = match tasks_by_id.get(assigned_task) {
                None => Some(WorkerCleanupReason::MissingTask),
                Some(task) if TaskLifecycle::is_terminal(task.status) => {
                    Some(WorkerCleanupReason::TerminalTask)
                }
                Some(task) if task.assignee.as_deref() != Some(worker.worker_id.as_str()) => {
                    Some(WorkerCleanupReason::TaskAssigneeMismatch)
                }
                _ => None,
            };

            if let Some(reason) = clear_reason {
                plan.add_worker_cleanup(WorkerCleanupAction::ClearAssignment {
                    worker_id: worker.worker_id.clone(),
                    reason,
                });
            }
        }
    }

    fn reconcile_claims(&self, input: &ReconciliationInput, plan: &mut ReconciliationPlan) {
        for run in input
            .active_runs
            .iter()
            .filter(|run| run.status.has_active_claim())
        {
            let expired = run.claim_is_expired_at(input.now);
            let session_alive = run
                .session_id
                .as_ref()
                .is_some_and(|session| input.session_is_alive(session));
            let active_operation = session_alive && input.session_has_active_operation_for_run(run);

            if expired && active_operation && self.config.renew_live_expired_claims {
                plan.add_run_renewal(RunRenewalAction {
                    run: run.clone(),
                    observed_at: input.now,
                    reason: RunRepairReason::ExpiredClaimWithLiveOperation,
                });
                continue;
            }

            if expired || !session_alive {
                match run.status {
                    RunStatus::Claimed => plan.add_run_release(RunReleaseAction {
                        run: run.clone(),
                        observed_at: input.now,
                        reason: if expired {
                            RunRepairReason::ExpiredClaim
                        } else {
                            RunRepairReason::DeadOrMissingSession
                        },
                    }),
                    RunStatus::Running => plan.add_run_failure(RunFailureAction {
                        run: run.clone(),
                        observed_at: input.now,
                        reason: if expired {
                            RunRepairReason::ExpiredRunningClaim
                        } else {
                            RunRepairReason::DeadOrMissingSession
                        },
                    }),
                    _ => {}
                }
            }
        }
    }

    fn reconcile_task_run_mismatches(
        &self,
        input: &ReconciliationInput,
        plan: &mut ReconciliationPlan,
    ) {
        let runs_by_task: HashMap<TaskId, Vec<&RunRecord>> = {
            let mut map: HashMap<TaskId, Vec<&RunRecord>> = HashMap::new();
            for run in input
                .active_runs
                .iter()
                .filter(|run| run.is_active() && retry_queued_is_actionable(run, input.now))
            {
                map.entry(run.task_id.clone()).or_default().push(run);
            }
            map
        };

        for task in &input.tasks {
            let task_runs = runs_by_task.get(&task.id).cloned().unwrap_or_default();

            if TaskLifecycle::is_terminal(task.status) {
                for run in task_runs {
                    plan.add_run_failure(RunFailureAction {
                        run: run.clone(),
                        observed_at: input.now,
                        reason: RunRepairReason::TerminalTaskHasActiveRun,
                    });
                }
                continue;
            }

            if self.config.require_active_worker_runs
                && is_worker_owned_status(task.status)
                && task.assignee.is_some()
                && !task_runs.iter().any(|run| run.role == RunRole::Worker)
            {
                plan.add_task_update(TaskUpdateAction::UnassignTask {
                    task_id: task.id.clone(),
                    worker_id: task.assignee.clone().map(WorkerId::new),
                    reason: TaskRepairReason::MissingActiveRun,
                });
            }

            if let Some(expected_role) = expected_active_run_role(task.status) {
                for run in task_runs.iter().filter(|run| run.role != expected_role) {
                    plan.add_escalation(ReconciliationEscalation {
                        task_id: Some(task.id.clone()),
                        run_id: Some(run.run_id.clone()),
                        reason: "run role mismatch for task status".to_string(),
                        context: format!(
                            "task {} is {:?}; active run {} has role {}",
                            task.id, task.status, run.run_id, run.role
                        ),
                    });
                }
            }

            if let Some(task_session) = task.session_id.as_deref() {
                for run in task_runs.iter().filter(|run| run.role == RunRole::Worker) {
                    let Some(run_session) = run.session_id.as_ref() else {
                        continue;
                    };
                    if run_session.as_str() != task_session {
                        plan.add_escalation(ReconciliationEscalation {
                            task_id: Some(task.id.clone()),
                            run_id: Some(run.run_id.clone()),
                            reason: "task session mismatch with active run".to_string(),
                            context: format!(
                                "task {} session {} differs from run {} session {}",
                                task.id, task_session, run.run_id, run_session
                            ),
                        });
                    }
                }
            }

            if is_worker_owned_status(task.status) {
                self.escalate_worker_pool_mismatch(input, plan, task);
            }
        }
    }

    fn escalate_worker_pool_mismatch(
        &self,
        input: &ReconciliationInput,
        plan: &mut ReconciliationPlan,
        task: &TaskEntry,
    ) {
        let Some(assignee) = task.assignee.as_ref() else {
            plan.add_escalation(ReconciliationEscalation {
                task_id: Some(task.id.clone()),
                run_id: None,
                reason: "worker-owned task has no assignee".to_string(),
                context: format!("task {} is {:?}", task.id, task.status),
            });
            return;
        };

        let Some(worker) = input.worker_by_id(assignee) else {
            return;
        };
        if !worker.is_alive {
            return;
        }

        if worker.assigned_task.as_ref() != Some(&task.id) {
            plan.add_escalation(ReconciliationEscalation {
                task_id: Some(task.id.clone()),
                run_id: None,
                reason: "worker pool assignment mismatch".to_string(),
                context: format!(
                    "task {} is assigned to worker {}, but worker assignment is {:?}",
                    task.id,
                    worker.worker_id,
                    worker.assigned_task.as_ref().map(TaskId::as_str)
                ),
            });
        }
    }
}

/// Immutable reconciliation input.
#[derive(Debug, Clone)]
pub struct ReconciliationInput {
    pub now: DateTime<Utc>,
    pub tasks: Vec<TaskEntry>,
    pub workers: Vec<WorkerSnapshotEntry>,
    pub dependencies: HashMap<TaskId, Vec<TaskId>>,
    pub git_ops: Option<GitOpsSnapshot>,
    pub active_runs: Vec<RunRecord>,
}

impl ReconciliationInput {
    /// Build an input snapshot from current in-memory projections.
    pub fn from_current_state(
        now: DateTime<Utc>,
        task_board: &TaskBoard,
        worker_pool: &WorkerPool,
        dependency_graph: &DependencyGraph,
        active_runs: Vec<RunRecord>,
    ) -> Self {
        let tasks = task_board.all_tasks();
        let dependencies = tasks
            .iter()
            .map(|task| {
                let mut deps: Vec<_> = dependency_graph
                    .get_dependencies(&task.id)
                    .into_iter()
                    .collect();
                deps.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                (task.id.clone(), deps)
            })
            .collect();

        Self {
            now,
            tasks,
            workers: worker_pool
                .all_workers()
                .map(WorkerSnapshotEntry::from_worker_info)
                .collect(),
            dependencies,
            git_ops: None,
            active_runs,
        }
    }

    fn session_is_alive(&self, session_id: &SessionId) -> bool {
        self.workers
            .iter()
            .any(|worker| worker.is_alive && &worker.session_id == session_id)
    }

    fn session_has_active_operation_for_run(&self, run: &RunRecord) -> bool {
        let Some(session_id) = run.session_id.as_ref() else {
            return false;
        };

        self.workers.iter().any(|worker| {
            worker.is_alive
                && &worker.session_id == session_id
                && worker.assigned_task.as_ref() == Some(&run.task_id)
        }) || self.tasks.iter().any(|task| {
            task.id == run.task_id
                && is_worker_owned_status(task.status)
                && task.session_id.as_deref() == Some(session_id.as_str())
        })
    }

    fn worker_by_id(&self, worker_id: &str) -> Option<&WorkerSnapshotEntry> {
        self.workers
            .iter()
            .find(|worker| worker.worker_id.as_str() == worker_id)
    }
}

/// Worker state captured without holding the worker-pool lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSnapshotEntry {
    pub worker_id: WorkerId,
    pub kind: WorkerKind,
    pub session_id: SessionId,
    pub assigned_task: Option<TaskId>,
    pub is_alive: bool,
}

impl WorkerSnapshotEntry {
    pub fn from_worker_info(worker: &WorkerInfo) -> Self {
        Self {
            worker_id: worker.id.clone(),
            kind: worker.kind,
            session_id: worker.session_id.clone(),
            assigned_task: worker.assigned_task.clone().map(TaskId::new),
            is_alive: worker.is_alive,
        }
    }
}

/// Snapshot of optional git-operation availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOpsSnapshot {
    pub available: bool,
}

/// Planned reconciliation output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconciliationPlan {
    pub task_updates: Vec<TaskUpdateAction>,
    pub run_retry_attempts: Vec<RunRetryAttemptAction>,
    pub run_renewals: Vec<RunRenewalAction>,
    pub run_releases: Vec<RunReleaseAction>,
    pub run_failures: Vec<RunFailureAction>,
    pub worker_cleanup_actions: Vec<WorkerCleanupAction>,
    pub escalations: Vec<ReconciliationEscalation>,
}

impl ReconciliationPlan {
    pub fn is_empty(&self) -> bool {
        self.task_updates.is_empty()
            && self.run_retry_attempts.is_empty()
            && self.run_renewals.is_empty()
            && self.run_releases.is_empty()
            && self.run_failures.is_empty()
            && self.worker_cleanup_actions.is_empty()
            && self.escalations.is_empty()
    }

    fn add_task_update(&mut self, action: TaskUpdateAction) {
        let task_id = action.task_id().clone();
        if !self
            .task_updates
            .iter()
            .any(|existing| existing.task_id() == &task_id)
        {
            self.task_updates.push(action);
        }
    }

    fn add_run_renewal(&mut self, action: RunRenewalAction) {
        if self.has_terminal_run_action(&action.run.run_id) {
            return;
        }
        if !self
            .run_renewals
            .iter()
            .any(|existing| existing.run.run_id == action.run.run_id)
        {
            self.run_renewals.push(action);
        }
    }

    fn add_run_release(&mut self, action: RunReleaseAction) {
        if self.has_terminal_run_action(&action.run.run_id) {
            return;
        }
        if !self
            .run_releases
            .iter()
            .any(|existing| existing.run.run_id == action.run.run_id)
        {
            self.run_releases.push(action);
        }
    }

    fn add_run_failure(&mut self, action: RunFailureAction) {
        if self
            .run_failures
            .iter()
            .any(|existing| existing.run.run_id == action.run.run_id)
        {
            return;
        }
        self.run_releases
            .retain(|existing| existing.run.run_id != action.run.run_id);
        self.run_retry_attempts
            .retain(|existing| existing.run.run_id != action.run.run_id);
        self.run_failures.push(action);
    }

    fn add_run_retry_attempt(&mut self, action: RunRetryAttemptAction) {
        if self.has_terminal_run_action(&action.run.run_id) {
            return;
        }
        if !self
            .run_retry_attempts
            .iter()
            .any(|existing| existing.run.run_id == action.run.run_id)
        {
            self.run_retry_attempts.push(action);
        }
    }

    fn add_worker_cleanup(&mut self, action: WorkerCleanupAction) {
        if !self
            .worker_cleanup_actions
            .iter()
            .any(|existing| existing.same_target(&action))
        {
            self.worker_cleanup_actions.push(action);
        }
    }

    fn add_escalation(&mut self, escalation: ReconciliationEscalation) {
        if !self.escalations.iter().any(|existing| {
            existing.task_id == escalation.task_id
                && existing.run_id == escalation.run_id
                && existing.reason == escalation.reason
        }) {
            self.escalations.push(escalation);
        }
    }

    fn has_terminal_run_action(&self, run_id: &RunId) -> bool {
        self.run_failures
            .iter()
            .any(|existing| &existing.run.run_id == run_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskUpdateAction {
    UnassignTask {
        task_id: TaskId,
        worker_id: Option<WorkerId>,
        reason: TaskRepairReason,
    },
}

impl TaskUpdateAction {
    pub fn task_id(&self) -> &TaskId {
        match self {
            Self::UnassignTask { task_id, .. } => task_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRepairReason {
    MissingWorker,
    DeadWorker,
    StaleSession,
    MissingActiveRun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRenewalAction {
    pub run: RunRecord,
    pub observed_at: DateTime<Utc>,
    pub reason: RunRepairReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRetryAttemptAction {
    pub run: RunRecord,
    pub observed_at: DateTime<Utc>,
    pub reason: RunRepairReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReleaseAction {
    pub run: RunRecord,
    pub observed_at: DateTime<Utc>,
    pub reason: RunRepairReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFailureAction {
    pub run: RunRecord,
    pub observed_at: DateTime<Utc>,
    pub reason: RunRepairReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunRepairReason {
    ExpiredClaim,
    ExpiredRunningClaim,
    ExpiredClaimWithLiveOperation,
    DeadOrMissingSession,
    TerminalTaskHasActiveRun,
    RetryDue,
}

impl RunRepairReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExpiredClaim => "expired claim",
            Self::ExpiredRunningClaim => "expired running claim",
            Self::ExpiredClaimWithLiveOperation => "expired claim with live operation",
            Self::DeadOrMissingSession => "dead or missing session",
            Self::TerminalTaskHasActiveRun => "terminal task has active run",
            Self::RetryDue => "retry due",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerCleanupAction {
    HandleWorkerDeath {
        session_id: SessionId,
    },
    ClearAssignment {
        worker_id: WorkerId,
        reason: WorkerCleanupReason,
    },
}

impl WorkerCleanupAction {
    fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::HandleWorkerDeath { session_id: left },
                Self::HandleWorkerDeath { session_id: right },
            ) => left == right,
            (
                Self::ClearAssignment {
                    worker_id: left, ..
                },
                Self::ClearAssignment {
                    worker_id: right, ..
                },
            ) => left == right,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCleanupReason {
    MissingTask,
    TerminalTask,
    TaskAssigneeMismatch,
    TaskSessionMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationEscalation {
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub reason: String,
    pub context: String,
}

fn is_worker_owned_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Assigned | TaskStatus::InProgress | TaskStatus::ChangesRequested
    )
}

fn expected_active_run_role(status: TaskStatus) -> Option<RunRole> {
    match status {
        TaskStatus::Assigned | TaskStatus::InProgress | TaskStatus::ChangesRequested => {
            Some(RunRole::Worker)
        }
        TaskStatus::InReview => Some(RunRole::Reviewer),
        TaskStatus::Approved => Some(RunRole::Integration),
        TaskStatus::Pending | TaskStatus::Blocked | TaskStatus::Merged => None,
    }
}

fn retry_queued_is_actionable(run: &RunRecord, now: DateTime<Utc>) -> bool {
    run.status != RunStatus::RetryQueued || run.retry_is_due_at(now)
}

fn task_session_is_stale(task: &TaskEntry, worker: &WorkerSnapshotEntry) -> bool {
    task.session_id
        .as_deref()
        .is_some_and(|current| current != worker.session_id.as_str())
}

fn map_run_store_error(err: RunStoreError) -> OrchestratorError {
    match err {
        RunStoreError::Storage(message) | RunStoreError::Serialization(message) => {
            OrchestratorError::StorageError(message)
        }
        other => OrchestratorError::PortError(other.to_string()),
    }
}
