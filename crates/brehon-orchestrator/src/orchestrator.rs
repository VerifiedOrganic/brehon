//! Main orchestrator implementation.
//!
//! The orchestrator coordinates the task board, dependency graph, worker pool,
//! and assignment engine to manage task execution.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::{debug, error, info, warn};

use brehon_ports::{AgentGateway, DecisionEngine, EventStore, GitOperations, RunStore};
use brehon_types::{
    ContinuationPolicyConfig, Event, EventId, EventKind, RetentionConfig, RetryPolicyConfig,
    SessionId, StabilityCounters, TaskId, TaskStatus, DEFAULT_RETENTION_SWEEP_INTERVAL_SECS,
};

use crate::assignment::AssignmentEngine;
use crate::dependency_graph::DependencyGraph;
use crate::error::{OrchestratorError, Result};
use crate::reconciler::Reconciler;
use crate::task_board::TaskBoard;
use crate::task_lifecycle::{TaskLifecycle, Transition};
use crate::worker_pool::{WorkerKind, WorkerPool, WorkerPoolConfig};

const DEFAULT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    pub worker_config: WorkerPoolConfig,
    pub reviewer_config: WorkerPoolConfig,
    pub dispatch_parallelism: usize,
    pub worktree_isolation: bool,
    pub branch_prefix: String,
    pub auto_cleanup_worktrees: bool,
    pub poll_interval: std::time::Duration,
    pub retention: RetentionConfig,
    pub retry_policy: RetryPolicyConfig,
    pub continuation_policy: ContinuationPolicyConfig,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            worker_config: WorkerPoolConfig::default(),
            reviewer_config: WorkerPoolConfig {
                kind: WorkerKind::Reviewer,
                ..Default::default()
            },
            dispatch_parallelism: 5,
            worktree_isolation: true,
            branch_prefix: "brehon/".to_string(),
            auto_cleanup_worktrees: true,
            poll_interval: DEFAULT_POLL_INTERVAL,
            retention: RetentionConfig {
                max_events: None,
                idempotency_ttl_hours: None,
                max_completed_tasks: 10_000,
                max_assignment_history: 1_000,
                max_tasks: 10_000,
                sweep_interval_secs: DEFAULT_RETENTION_SWEEP_INTERVAL_SECS,
            },
            retry_policy: RetryPolicyConfig::default(),
            continuation_policy: ContinuationPolicyConfig::default(),
        }
    }
}

pub struct OrchestratorDeps {
    pub event_store: Arc<dyn EventStore>,
    pub gateway: Arc<dyn AgentGateway>,
    pub git_ops: Option<Arc<dyn GitOperations>>,
    pub decision_engine: Option<Arc<dyn DecisionEngine>>,
}

pub struct Orchestrator {
    pub(crate) config: OrchestratorConfig,
    pub(crate) deps: OrchestratorDeps,
    pub(crate) task_board: TaskBoard,
    pub(crate) dependency_graph: DependencyGraph,
    pub(crate) worker_pool: Arc<parking_lot::RwLock<WorkerPool>>,
    pub(crate) run_store: Option<Arc<dyn RunStore>>,
    pub(crate) reconciler: Reconciler,
    assignment_engine: AssignmentEngine,
    running: bool,
    last_processed_event: Option<EventId>,
    completed_tasks: HashSet<TaskId>,
    last_retention_sweep: Option<tokio::time::Instant>,
}

impl Orchestrator {
    pub fn new(config: OrchestratorConfig, deps: OrchestratorDeps) -> Self {
        let max_tasks = config.retention.max_tasks as usize;
        let max_assignment_history = config.retention.max_assignment_history as usize;

        let worker_pool = Arc::new(parking_lot::RwLock::new(
            WorkerPool::with_max_assignment_history(
                config.worker_config.clone(),
                deps.gateway.clone(),
                max_assignment_history,
            ),
        ));

        let assignment_engine = AssignmentEngine::new(
            worker_pool.clone(),
            deps.gateway.clone(),
            deps.decision_engine.clone(),
        );

        Self {
            config,
            deps,
            task_board: TaskBoard::with_max_tasks(max_tasks),
            dependency_graph: DependencyGraph::new(),
            worker_pool,
            run_store: None,
            reconciler: Reconciler::new(),
            assignment_engine,
            running: false,
            last_processed_event: None,
            completed_tasks: HashSet::new(),
            last_retention_sweep: None,
        }
    }

    pub fn set_run_store(&mut self, run_store: Arc<dyn RunStore>) {
        self.run_store = Some(run_store);
    }

    pub async fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<()> {
        info!("Starting orchestrator");
        self.running = true;

        self.run_startup_reconciliation().await?;
        self.spawn_workers_to_min().await?;

        while self.running && !shutdown.load(Ordering::SeqCst) {
            if let Err(e) = self.tick().await {
                error!(error = ?e, "Error in orchestrator tick");
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }

        info!("Orchestrator stopped");
        Ok(())
    }

    pub(crate) async fn tick(&mut self) -> Result<()> {
        let events = self
            .deps
            .event_store
            .stream(self.last_processed_event, 100)
            .await?;

        for (event, event_id) in &events {
            self.process_event(event, *event_id);
        }
        if let Some((_, last_event_id)) = events.last() {
            self.last_processed_event = Some(*last_event_id);
        }

        // Tick order: process events -> reconcile -> update dependencies -> dispatch -> bounds -> retention.
        self.reconcile_before_dispatch("tick").await?;
        self.continue_active_runs().await?;
        self.update_dependencies()?;
        self.dispatch_ready_tasks().await?;
        self.apply_bounds();
        self.sweep_retention().await;

        Ok(())
    }

    /// Periodic sweep of hot event log and idempotency keys.
    /// Rate-limited by configured wall-clock interval to avoid excessive I/O.
    async fn sweep_retention(&mut self) {
        // Keep this guard in sync when adding new retention dimensions.
        if self.config.retention.max_events.is_none()
            && self.config.retention.idempotency_ttl_hours.is_none()
        {
            return;
        }

        // RetentionConfig::default() uses 0 as a merge sentinel; treat that as
        // the documented operational default here.
        let sweep_interval_secs = if self.config.retention.sweep_interval_secs == 0 {
            DEFAULT_RETENTION_SWEEP_INTERVAL_SECS
        } else {
            self.config.retention.sweep_interval_secs
        };

        let now = tokio::time::Instant::now();
        if let Some(last_sweep) = self.last_retention_sweep {
            let interval = tokio::time::Duration::from_secs(sweep_interval_secs);
            if now.duration_since(last_sweep) < interval {
                return;
            }
        }

        let mut sweep_had_errors = false;

        if let Some(max_events) = self.config.retention.max_events {
            match self.deps.event_store.high_water_mark().await {
                Ok(high_water) => {
                    let high = high_water.as_u64();
                    if high > max_events {
                        // retain_events(before) archives events with seq < before.
                        // To keep exactly max_events events (seq high-max_events+1 .. high),
                        // we set before = high - max_events + 1.
                        let mut before_seq = high.saturating_sub(max_events).saturating_add(1);

                        // Never archive events that haven't been processed yet.
                        // If we have not processed anything yet, safe_before=1 prevents archiving.
                        let safe_before = self
                            .last_processed_event
                            .map(|last| last.as_u64().saturating_add(1))
                            .unwrap_or(1);
                        before_seq = std::cmp::min(before_seq, safe_before);

                        if before_seq > 1 {
                            let before = EventId::new(before_seq);
                            match self.deps.event_store.retain_events(before).await {
                                Ok(archived) => {
                                    if archived > 0 {
                                        info!(archived, before = %before, "Archived old events");
                                    }
                                }
                                Err(e) => {
                                    sweep_had_errors = true;
                                    warn!(error = ?e, "Failed to retain events");
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    sweep_had_errors = true;
                    warn!(error = ?e, "Failed to read high water mark for retention");
                }
            }
        }

        if let Some(ttl_hours) = self.config.retention.idempotency_ttl_hours {
            let older_than = tokio::time::Duration::from_secs(ttl_hours.saturating_mul(3600));
            match self
                .deps
                .event_store
                .expire_idempotency_keys(older_than)
                .await
            {
                Ok(expired) => {
                    if expired > 0 {
                        info!(expired, ttl_hours, "Expired stale idempotency keys");
                    }
                }
                Err(e) => {
                    sweep_had_errors = true;
                    warn!(error = ?e, "Failed to expire idempotency keys");
                }
            }
        }

        if !sweep_had_errors {
            self.last_retention_sweep = Some(now);
        }
    }

    #[allow(dead_code)]
    fn process_event(&mut self, event: &Event, _event_id: EventId) {
        match &event.kind {
            EventKind::TaskCreated { task_id } => {
                let id = TaskId::new(task_id);
                if !self.task_board.has_task(&id) {
                    let entry =
                        crate::task_board::TaskEntry::new(id.clone(), String::new(), String::new());
                    self.task_board.add_task(entry);
                    self.dependency_graph.add_task(id);
                    debug!(task_id = %task_id, "Task created");
                }
            }
            EventKind::TaskAssigned { task_id, agent_id } => {
                if let Some(_task) = self.task_board.get_task(&TaskId::new(task_id)) {
                    self.task_board
                        .assign_task(&TaskId::new(task_id), agent_id, None)
                        .ok();
                    debug!(task_id = %task_id, agent_id = %agent_id, "Task assigned");
                }
            }
            EventKind::TaskCompleted { task_id } => {
                if let Some(task) = self.task_board.get_task(&TaskId::new(task_id)) {
                    if let Ok(new_status) = TaskLifecycle::apply(task.status, Transition::Complete)
                    {
                        self.task_board
                            .update_task_status(&TaskId::new(task_id), new_status)
                            .ok();
                        debug!(task_id = %task_id, "Task completed");
                    }
                }
            }
            EventKind::AgentDied { session_id, .. } => {
                let session = SessionId::new(session_id);
                if let Some(worker) = self.worker_pool.read().get_worker_by_session(&session) {
                    let worker_id = worker.id.clone();
                    if let Ok(affected_tasks) =
                        self.assignment_engine.handle_worker_death(&worker_id)
                    {
                        for task_id in affected_tasks {
                            self.task_board.unassign_task(&task_id).ok();
                            debug!(task_id = %task_id, "Task unassigned due to agent death");
                        }
                    }
                }
            }
            EventKind::MergeCommitted { task_id } => {
                let id = TaskId::new(task_id);
                self.completed_tasks.insert(id.clone());
                self.task_board
                    .update_task_status(&id, TaskStatus::Merged)
                    .ok();
                debug!(task_id = %task_id, "Task merged");
            }
            EventKind::MergeAborted { task_id, reason } => {
                if let Some(task) = self.task_board.get_task(&TaskId::new(task_id)) {
                    if let Ok(new_status) = TaskLifecycle::apply(task.status, Transition::Assign) {
                        self.task_board
                            .update_task_status(&TaskId::new(task_id), new_status)
                            .ok();
                    }
                }
                warn!(task_id = %task_id, reason = %reason, "Merge aborted");
            }
            _ => {}
        }
    }

    fn update_dependencies(&mut self) -> Result<()> {
        self.dependency_graph
            .update_statuses(&self.completed_tasks)?;
        Ok(())
    }

    async fn dispatch_ready_tasks(&mut self) -> Result<()> {
        let assignable_tasks = self.task_board.get_assignable_tasks();

        for task in assignable_tasks {
            if !self
                .dependency_graph
                .is_ready(&task.id, &self.completed_tasks)
            {
                let unmet = self.dependency_graph.get_unmet_dependencies(&task.id);
                self.task_board.set_blocked(&task.id, unmet).ok();
                continue;
            }

            if self.worker_pool.read().available_count() == 0 {
                break;
            }

            if self.task_has_pending_retry(&task.id).await? {
                debug!(task_id = %task.id, "Skipping dispatch while retry is not due");
                continue;
            }

            if !self.check_git_available(task.id.as_str()) {
                warn!(
                    task_id = %task.id,
                    "Git operations unavailable, cannot dispatch live task"
                );
                continue;
            }

            match self.assignment_engine.assign_task(&task).await {
                Ok(assignment) => {
                    if let Err(e) = self
                        .assignment_engine
                        .dispatch_task(&assignment, &task)
                        .await
                    {
                        error!(task_id = %task.id, error = ?e, "Failed to dispatch task");
                        self.task_board.unassign_task(&task.id).ok();
                    } else {
                        self.task_board
                            .update_task_status(&task.id, TaskStatus::Assigned)
                            .ok();
                        self.task_board
                            .assign_task(
                                &task.id,
                                assignment.worker_id.as_str(),
                                Some(assignment.session_id.as_str()),
                            )
                            .ok();
                    }
                }
                Err(OrchestratorError::NoAvailableWorkers) => {
                    break;
                }
                Err(e) => {
                    error!(task_id = %task.id, error = ?e, "Failed to assign task");
                }
            }
        }

        Ok(())
    }

    async fn task_has_pending_retry(&self, task_id: &TaskId) -> Result<bool> {
        let Some(run_store) = self.run_store.as_ref() else {
            return Ok(false);
        };
        let now = chrono::Utc::now();
        let runs = run_store
            .runs_for_task(task_id)
            .await
            .map_err(|err| OrchestratorError::PortError(err.to_string()))?;
        Ok(runs.iter().any(|run| {
            run.status == brehon_types::RunStatus::RetryQueued && !run.retry_is_due_at(now)
        }))
    }

    fn check_git_available(&self, _task_id: &str) -> bool {
        if !self.config.worktree_isolation {
            return true;
        }

        self.deps.git_ops.is_some()
    }

    pub fn add_task(
        &mut self,
        task_id: TaskId,
        title: String,
        description: String,
        dependencies: Vec<TaskId>,
    ) {
        let mut entry = crate::task_board::TaskEntry::new(task_id.clone(), title, description);

        for dep in &dependencies {
            self.dependency_graph
                .add_dependency(task_id.clone(), dep.clone())
                .ok();
        }

        entry.dependencies = dependencies;
        self.task_board.add_task(entry);
    }

    pub fn add_dependency(&mut self, task_id: TaskId, depends_on: TaskId) -> Result<()> {
        self.dependency_graph.add_dependency(task_id, depends_on)
    }

    pub fn stop(&mut self) {
        info!("Stopping orchestrator");
        self.running = false;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn task_board(&self) -> &TaskBoard {
        &self.task_board
    }

    pub fn dependency_graph(&self) -> &DependencyGraph {
        &self.dependency_graph
    }

    pub fn worker_pool(&self) -> &Arc<parking_lot::RwLock<WorkerPool>> {
        &self.worker_pool
    }

    pub fn completed_tasks(&self) -> &HashSet<TaskId> {
        &self.completed_tasks
    }

    /// Derive a stability counter snapshot from internal data structures.
    ///
    /// The snapshot is taken under individual fine-grained locks so it does not
    /// block the assignment engine or worker pool for the duration of the read.
    pub fn stability_counters(&self) -> StabilityCounters {
        StabilityCounters {
            pending_requests: 0,       // ACP-level; populated by gateway aggregation
            pending_prompt_waiters: 0, // ACP-level; populated by gateway aggregation
            active_reviews: 0,         // Review-level; populated by coordinator aggregation
            completed_tasks: self.completed_tasks.len(),
            assignment_history: self.assignment_engine.history_len(),
            blocked_sends: 0, // ACP-level; populated by gateway aggregation
            tokens_used: 0,   // Adapter-level; populated by gateway aggregation
        }
    }

    /// Apply retention and boundedness policies to in-memory collections.
    pub fn apply_bounds(&mut self) {
        self.task_board.apply_bounds();

        // Prune completed_tasks that have no active dependents.
        let max_completed_tasks = self.config.retention.max_completed_tasks as usize;
        if self.completed_tasks.len() > max_completed_tasks {
            let to_remove = self.completed_tasks.len() - max_completed_tasks;
            let candidates: Vec<TaskId> = self
                .completed_tasks
                .iter()
                .filter(|task_id| self.dependency_graph.get_dependents(task_id).is_empty())
                .cloned()
                .collect();
            for task_id in candidates.into_iter().take(to_remove) {
                self.completed_tasks.remove(&task_id);
            }
        }
    }
}

impl DependencyGraph {
    fn is_ready(&self, task_id: &TaskId, completed: &HashSet<TaskId>) -> bool {
        let dependencies = self.get_dependencies(task_id);
        dependencies.iter().all(|dep| completed.contains(dep))
    }

    fn get_unmet_dependencies(&self, task_id: &TaskId) -> Vec<TaskId> {
        self.get_dependencies(task_id).into_iter().collect()
    }

    fn update_statuses(&mut self, _completed: &HashSet<TaskId>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use brehon_ports::{EventStore, PortError};
    use brehon_test_harness::{InMemoryEventStore, MockGateway};
    use brehon_types::{ClaimId, EventFilter, QueueClaim, TaskStatus, ViewUpdate};

    #[allow(dead_code)]
    fn create_event(kind: EventKind, aggregate_id: &str) -> (Event, EventId) {
        (
            Event {
                kind,
                timestamp: chrono::Utc::now(),
                aggregate_id: aggregate_id.to_string(),
            },
            EventId::new(1),
        )
    }

    #[derive(Debug)]
    struct FailOnceExpireEventStore {
        inner: InMemoryEventStore,
        fail_once: AtomicBool,
        expire_calls: AtomicUsize,
    }

    impl FailOnceExpireEventStore {
        fn new() -> Self {
            Self {
                inner: InMemoryEventStore::new(),
                fail_once: AtomicBool::new(true),
                expire_calls: AtomicUsize::new(0),
            }
        }

        fn expire_calls(&self) -> usize {
            self.expire_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl EventStore for FailOnceExpireEventStore {
        async fn append(&self, event: Event) -> std::result::Result<EventId, PortError> {
            self.inner.append(event).await
        }

        async fn append_atomic(
            &self,
            events: Vec<Event>,
            views: Vec<ViewUpdate>,
        ) -> std::result::Result<Vec<EventId>, PortError> {
            self.inner.append_atomic(events, views).await
        }

        async fn append_and_enqueue(
            &self,
            event: Event,
            queue: &str,
            item_id: &str,
            idempotency_key: Option<&str>,
        ) -> std::result::Result<EventId, PortError> {
            self.inner
                .append_and_enqueue(event, queue, item_id, idempotency_key)
                .await
        }

        async fn query(&self, filter: EventFilter) -> std::result::Result<Vec<Event>, PortError> {
            self.inner.query(filter).await
        }

        async fn stream(
            &self,
            since: Option<EventId>,
            limit: usize,
        ) -> std::result::Result<Vec<(Event, EventId)>, PortError> {
            self.inner.stream(since, limit).await
        }

        async fn claim_next(
            &self,
            queue: &str,
            consumer: &str,
            lease_for: std::time::Duration,
        ) -> std::result::Result<Option<QueueClaim>, PortError> {
            self.inner.claim_next(queue, consumer, lease_for).await
        }

        async fn ack_claim(&self, claim_id: &ClaimId) -> std::result::Result<(), PortError> {
            self.inner.ack_claim(claim_id).await
        }

        async fn renew_claim(
            &self,
            claim_id: &ClaimId,
            lease_for: std::time::Duration,
        ) -> std::result::Result<(), PortError> {
            self.inner.renew_claim(claim_id, lease_for).await
        }

        async fn high_water_mark(&self) -> std::result::Result<EventId, PortError> {
            self.inner.high_water_mark().await
        }

        async fn retain_events(&self, before: EventId) -> std::result::Result<usize, PortError> {
            self.inner.retain_events(before).await
        }

        async fn expire_idempotency_keys(
            &self,
            older_than: std::time::Duration,
        ) -> std::result::Result<usize, PortError> {
            self.expire_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err(PortError::Storage(
                    "injected expire_idempotency_keys failure".into(),
                ));
            }
            self.inner.expire_idempotency_keys(older_than).await
        }
    }

    #[tokio::test]
    async fn orchestrator_new() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };

        let orchestrator = Orchestrator::new(config, deps);

        assert!(!orchestrator.is_running());
        assert!(orchestrator.task_board().is_empty());
    }

    #[tokio::test]
    async fn add_task() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };

        let mut orchestrator = Orchestrator::new(config, deps);

        orchestrator.add_task(
            TaskId::new("T001"),
            "Test task".into(),
            "Description".into(),
            vec![],
        );

        assert_eq!(orchestrator.task_board().len(), 1);
        let task = orchestrator.task_board().get_task(&TaskId::new("T001"));
        assert!(task.is_some());
    }

    #[tokio::test]
    async fn add_dependency() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };

        let mut orchestrator = Orchestrator::new(config, deps);

        orchestrator.add_task(TaskId::new("T001"), "Task 1".into(), "Desc".into(), vec![]);
        orchestrator.add_task(TaskId::new("T002"), "Task 2".into(), "Desc".into(), vec![]);

        let result = orchestrator.add_dependency(TaskId::new("T002"), TaskId::new("T001"));
        assert!(result.is_ok());

        let deps = orchestrator
            .dependency_graph()
            .get_dependencies(&TaskId::new("T002"));
        assert!(deps.contains(&TaskId::new("T001")));
    }

    #[tokio::test]
    async fn cyclic_dependency_rejected() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };

        let mut orchestrator = Orchestrator::new(config, deps);

        orchestrator.add_task(TaskId::new("T001"), "Task 1".into(), "Desc".into(), vec![]);
        orchestrator.add_task(TaskId::new("T002"), "Task 2".into(), "Desc".into(), vec![]);

        orchestrator
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();

        let result = orchestrator.add_dependency(TaskId::new("T001"), TaskId::new("T002"));
        assert!(result.is_err());

        if let Err(OrchestratorError::CycleError(msg)) = result {
            assert!(msg.contains("cycle"));
        } else {
            panic!("Expected CycleError");
        }
    }

    #[tokio::test]
    async fn process_event_task_created() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };

        let mut orchestrator = Orchestrator::new(config, deps);

        let event = Event {
            kind: EventKind::TaskCreated {
                task_id: "T001".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "T001".into(),
        };

        orchestrator.process_event(&event, EventId::new(1));

        assert_eq!(orchestrator.task_board().len(), 1);
        assert!(orchestrator
            .dependency_graph()
            .has_task(&TaskId::new("T001")));
    }

    #[tokio::test]
    async fn lifecycle_transitions_invalid() {
        let result = TaskLifecycle::apply(TaskStatus::Pending, Transition::Merge);
        assert!(result.is_err());

        let result = TaskLifecycle::apply(TaskStatus::Merged, Transition::Assign);
        assert!(result.is_err());

        let result = TaskLifecycle::apply(TaskStatus::InProgress, Transition::Approve);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dependency_graph_topological_order() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };

        let mut orchestrator = Orchestrator::new(config, deps);

        orchestrator.add_task(TaskId::new("T001"), "Task 1".into(), "Desc".into(), vec![]);
        orchestrator.add_task(TaskId::new("T002"), "Task 2".into(), "Desc".into(), vec![]);
        orchestrator.add_task(TaskId::new("T003"), "Task 3".into(), "Desc".into(), vec![]);

        orchestrator
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();
        orchestrator
            .add_dependency(TaskId::new("T003"), TaskId::new("T002"))
            .unwrap();

        let order = orchestrator.dependency_graph().topological_order().unwrap();

        let t001_idx = order
            .iter()
            .position(|t| t == &TaskId::new("T001"))
            .unwrap();
        let t002_idx = order
            .iter()
            .position(|t| t == &TaskId::new("T002"))
            .unwrap();
        let t003_idx = order
            .iter()
            .position(|t| t == &TaskId::new("T003"))
            .unwrap();

        assert!(t001_idx < t002_idx);
        assert!(t002_idx < t003_idx);
    }

    #[tokio::test]
    async fn stability_counters_empty() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };

        let orchestrator = Orchestrator::new(config, deps);
        let counters = orchestrator.stability_counters();

        assert_eq!(counters.completed_tasks, 0);
        assert_eq!(counters.assignment_history, 0);
    }

    #[tokio::test]
    async fn tick_unassigns_tasks_owned_by_missing_workers() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store.clone(),
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T001".to_string(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "T001".to_string(),
            })
            .await
            .unwrap();

        store
            .append(Event {
                kind: EventKind::TaskAssigned {
                    task_id: "T001".to_string(),
                    agent_id: "dead-worker".to_string(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "T001".to_string(),
            })
            .await
            .unwrap();

        orchestrator.tick().await.unwrap();

        let task = orchestrator
            .task_board()
            .get_task(&TaskId::new("T001"))
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.assignee.is_none());
    }

    #[tokio::test]
    async fn tick_clears_stale_worker_assignments_when_task_is_no_longer_active() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        let worker_id = orchestrator.spawn_workers_to_min().await.unwrap()[0].clone();

        orchestrator.add_task(
            TaskId::new("T001"),
            "Task".to_string(),
            "Desc".to_string(),
            vec![],
        );
        orchestrator
            .task_board
            .assign_task(&TaskId::new("T001"), worker_id.as_str(), None)
            .unwrap();
        {
            let mut pool = orchestrator.worker_pool.write();
            pool.assign_task(&worker_id, "T001").unwrap();
        }

        orchestrator
            .task_board
            .unassign_task(&TaskId::new("T001"))
            .unwrap();

        orchestrator.tick().await.unwrap();

        let assignment = orchestrator
            .worker_pool
            .read()
            .get_worker(&worker_id)
            .unwrap()
            .assigned_task
            .clone();
        assert!(assignment.is_none());
    }

    #[tokio::test]
    async fn tick_unassigns_dead_worker_tasks_without_killing_live_replacement_session() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        let worker_id = orchestrator.spawn_workers_to_min().await.unwrap()[0].clone();
        let current_session = orchestrator
            .worker_pool
            .read()
            .get_worker(&worker_id)
            .unwrap()
            .session_id
            .clone();

        orchestrator.add_task(
            TaskId::new("T001"),
            "Task".to_string(),
            "Desc".to_string(),
            vec![],
        );
        orchestrator
            .task_board
            .assign_task(
                &TaskId::new("T001"),
                worker_id.as_str(),
                Some("stale-session"),
            )
            .unwrap();
        {
            let mut pool = orchestrator.worker_pool.write();
            pool.assign_task(&worker_id, "T001").unwrap();
        }

        orchestrator.tick().await.unwrap();

        let task = orchestrator
            .task_board
            .get_task(&TaskId::new("T001"))
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.assignee.is_none());

        let worker = orchestrator
            .worker_pool
            .read()
            .get_worker(&worker_id)
            .unwrap()
            .clone();
        assert!(
            worker.is_alive,
            "stale-session reconciliation must not kill live worker"
        );
        assert_eq!(worker.session_id, current_session);
        assert!(worker.assigned_task.is_none());
    }

    #[tokio::test]
    async fn tick_unassigns_tasks_for_workers_marked_dead_in_pool() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        let worker_id = orchestrator.spawn_workers_to_min().await.unwrap()[0].clone();
        {
            let mut pool = orchestrator.worker_pool.write();
            pool.assign_task(&worker_id, "T001").unwrap();
            pool.set_worker_alive_for_test(&worker_id, false).unwrap();
        }

        orchestrator.add_task(
            TaskId::new("T001"),
            "Task".to_string(),
            "Desc".to_string(),
            vec![],
        );
        orchestrator
            .task_board
            .assign_task(&TaskId::new("T001"), worker_id.as_str(), None)
            .unwrap();

        orchestrator.tick().await.unwrap();

        let task = orchestrator
            .task_board
            .get_task(&TaskId::new("T001"))
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.assignee.is_none());

        let alive_count = orchestrator.worker_pool.read().alive_count();
        assert_eq!(
            alive_count, 1,
            "dead-worker reconciliation should restore worker pool min_count"
        );

        orchestrator.tick().await.unwrap();
        let alive_count_after_second_tick = orchestrator.worker_pool.read().alive_count();
        assert_eq!(
            alive_count_after_second_tick, 1,
            "re-running reconciliation should be idempotent for already-dead workers"
        );
    }

    #[tokio::test]
    async fn tick_respawns_dead_idle_worker_to_maintain_min_count() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        let worker_id = orchestrator.spawn_workers_to_min().await.unwrap()[0].clone();
        {
            let mut pool = orchestrator.worker_pool.write();
            pool.set_worker_alive_for_test(&worker_id, false).unwrap();
        }

        orchestrator.tick().await.unwrap();

        let alive_count = orchestrator.worker_pool.read().alive_count();
        assert_eq!(
            alive_count, 1,
            "dead idle worker should still trigger respawn to keep min_count"
        );
    }

    #[tokio::test]
    async fn tick_respawns_dead_worker_with_review_owned_task_without_unassigning_task() {
        for status in [TaskStatus::InReview, TaskStatus::Approved] {
            let store = Arc::new(InMemoryEventStore::new());
            let gateway = Arc::new(MockGateway::new());

            let config = OrchestratorConfig::default();
            let deps = OrchestratorDeps {
                event_store: store,
                gateway,
                git_ops: None,
                decision_engine: None,
            };
            let mut orchestrator = Orchestrator::new(config, deps);

            let worker_id = orchestrator.spawn_workers_to_min().await.unwrap()[0].clone();

            orchestrator.add_task(
                TaskId::new("T001"),
                "Task".to_string(),
                "Desc".to_string(),
                vec![],
            );
            orchestrator
                .task_board
                .assign_task(&TaskId::new("T001"), worker_id.as_str(), None)
                .unwrap();
            orchestrator
                .task_board
                .update_task_status(&TaskId::new("T001"), status)
                .unwrap();
            {
                let mut pool = orchestrator.worker_pool.write();
                pool.assign_task(&worker_id, "T001").unwrap();
                pool.set_worker_alive_for_test(&worker_id, false).unwrap();
            }

            orchestrator.tick().await.unwrap();

            let task = orchestrator
                .task_board
                .get_task(&TaskId::new("T001"))
                .unwrap();
            assert_eq!(task.status, status);
            assert_eq!(task.assignee.as_deref(), Some(worker_id.as_str()));

            let worker = orchestrator
                .worker_pool
                .read()
                .get_worker(&worker_id)
                .unwrap()
                .clone();
            assert!(worker.assigned_task.is_none());

            let alive_count = orchestrator.worker_pool.read().alive_count();
            assert_eq!(
                alive_count, 1,
                "dead worker with review-owned task should still trigger respawn"
            );
        }
    }

    #[tokio::test]
    async fn tick_respawns_dead_worker_with_terminal_task_without_requeueing_terminal_state() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        let worker_id = orchestrator.spawn_workers_to_min().await.unwrap()[0].clone();

        orchestrator.add_task(
            TaskId::new("T001"),
            "Task".to_string(),
            "Desc".to_string(),
            vec![],
        );
        orchestrator
            .task_board
            .assign_task(&TaskId::new("T001"), worker_id.as_str(), None)
            .unwrap();
        orchestrator
            .task_board
            .update_task_status(&TaskId::new("T001"), TaskStatus::Merged)
            .unwrap();
        {
            let mut pool = orchestrator.worker_pool.write();
            pool.assign_task(&worker_id, "T001").unwrap();
            pool.set_worker_alive_for_test(&worker_id, false).unwrap();
        }

        orchestrator.tick().await.unwrap();

        let task = orchestrator
            .task_board
            .get_task(&TaskId::new("T001"))
            .unwrap();
        assert_eq!(task.status, TaskStatus::Merged);

        let worker = orchestrator
            .worker_pool
            .read()
            .get_worker(&worker_id)
            .unwrap()
            .clone();
        assert!(worker.assigned_task.is_none());

        let alive_count = orchestrator.worker_pool.read().alive_count();
        assert_eq!(
            alive_count, 1,
            "dead worker with terminal task should still trigger respawn"
        );
    }

    #[tokio::test]
    async fn tick_keeps_worker_assignment_for_non_terminal_in_review_task() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let config = OrchestratorConfig::default();
        let deps = OrchestratorDeps {
            event_store: store,
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        let worker_id = orchestrator.spawn_workers_to_min().await.unwrap()[0].clone();

        orchestrator.add_task(
            TaskId::new("T001"),
            "Task".to_string(),
            "Desc".to_string(),
            vec![],
        );
        orchestrator
            .task_board
            .assign_task(&TaskId::new("T001"), worker_id.as_str(), None)
            .unwrap();
        orchestrator
            .task_board
            .update_task_status(&TaskId::new("T001"), TaskStatus::InReview)
            .unwrap();
        {
            let mut pool = orchestrator.worker_pool.write();
            pool.assign_task(&worker_id, "T001").unwrap();
        }

        orchestrator.tick().await.unwrap();

        let assignment = orchestrator
            .worker_pool
            .read()
            .get_worker(&worker_id)
            .unwrap()
            .assigned_task
            .clone();
        assert_eq!(assignment.as_deref(), Some("T001"));
    }

    #[tokio::test]
    async fn retention_sweep_does_not_archive_before_first_processed_event() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let mut config = OrchestratorConfig::default();
        config.retention.max_events = Some(5);
        config.retention.sweep_interval_secs = 1;

        let deps = OrchestratorDeps {
            event_store: store.clone(),
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        for i in 0..10 {
            let task_id = format!("T{:03}", i);
            store
                .append(Event {
                    kind: EventKind::TaskCreated {
                        task_id: task_id.clone(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: task_id,
                })
                .await
                .unwrap();
        }

        assert_eq!(store.len(), 10);
        orchestrator.sweep_retention().await;
        assert_eq!(
            store.len(),
            10,
            "retention sweep must not archive when no event has been processed yet"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retention_sweep_respects_configured_interval() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let mut config = OrchestratorConfig::default();
        config.retention.max_events = Some(5);
        config.retention.sweep_interval_secs = 3600;

        let deps = OrchestratorDeps {
            event_store: store.clone(),
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        for i in 0..10 {
            let task_id = format!("T{:03}", i);
            store
                .append(Event {
                    kind: EventKind::TaskCreated {
                        task_id: task_id.clone(),
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: task_id,
                })
                .await
                .unwrap();
        }

        orchestrator.last_processed_event = Some(EventId::new(10));
        orchestrator.sweep_retention().await;
        assert_eq!(store.len(), 5);

        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T010".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "T010".into(),
            })
            .await
            .unwrap();
        assert_eq!(store.len(), 6);

        orchestrator.last_processed_event = Some(EventId::new(11));
        orchestrator.sweep_retention().await;
        assert_eq!(
            store.len(),
            6,
            "subsequent sweeps within sweep_interval_secs should be skipped"
        );

        tokio::time::advance(tokio::time::Duration::from_secs(3600)).await;

        store
            .append(Event {
                kind: EventKind::TaskCreated {
                    task_id: "T011".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "T011".into(),
            })
            .await
            .unwrap();
        assert_eq!(store.len(), 7);

        orchestrator.last_processed_event = Some(EventId::new(12));
        orchestrator.sweep_retention().await;
        assert_eq!(
            store.len(),
            5,
            "sweep should run once sweep_interval_secs has elapsed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retention_sweep_retries_immediately_after_failed_sweep() {
        let store = Arc::new(FailOnceExpireEventStore::new());
        let gateway = Arc::new(MockGateway::new());

        let mut config = OrchestratorConfig::default();
        config.retention.idempotency_ttl_hours = Some(1);
        config.retention.sweep_interval_secs = 3600;

        let deps = OrchestratorDeps {
            event_store: store.clone(),
            gateway,
            git_ops: None,
            decision_engine: None,
        };
        let mut orchestrator = Orchestrator::new(config, deps);

        orchestrator.sweep_retention().await;
        assert_eq!(store.expire_calls(), 1);
        assert!(
            orchestrator.last_retention_sweep.is_none(),
            "failed sweeps should not update last_retention_sweep"
        );

        orchestrator.sweep_retention().await;
        assert_eq!(
            store.expire_calls(),
            2,
            "failed sweeps should be retried immediately instead of being interval-throttled"
        );
        assert!(
            orchestrator.last_retention_sweep.is_some(),
            "successful retry should update last_retention_sweep"
        );
    }
}
