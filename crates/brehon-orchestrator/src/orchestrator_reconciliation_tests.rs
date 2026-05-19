use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};

use brehon_ports::{AgentGateway, ClaimRequest, EventStore, PortError, RunStore};
use brehon_test_harness::{InMemoryEventStore, InMemoryRunStore, MockGateway};
use brehon_types::{
    AgentCapabilities, ClaimOwner, Event, EventKind, HealthStatus, PromptHandle, PromptId,
    PromptTurn, RunId, RunRecord, RunRole, RunStatus, SessionId, SessionInfo, SessionSpec, TaskId,
    TaskStatus, TerminalId,
};

use crate::orchestrator::{Orchestrator, OrchestratorConfig, OrchestratorDeps};

#[tokio::test]
async fn tick_reconciliation_happens_before_dispatch() {
    let store = Arc::new(InMemoryEventStore::new());
    let gateway = Arc::new(MockGateway::new());

    let mut config = OrchestratorConfig::default();
    config.worktree_isolation = false;

    let deps = OrchestratorDeps {
        event_store: store.clone(),
        gateway: gateway.clone(),
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.spawn_workers_to_min().await.unwrap();

    append_task_event(
        &store,
        EventKind::TaskCreated {
            task_id: "T001".to_string(),
        },
    )
    .await;
    append_task_event(
        &store,
        EventKind::TaskAssigned {
            task_id: "T001".to_string(),
            agent_id: "dead-worker".to_string(),
        },
    )
    .await;

    orchestrator.tick().await.unwrap();

    let task = orchestrator
        .task_board()
        .get_task(&TaskId::new("T001"))
        .unwrap();
    assert_eq!(task.status, TaskStatus::Assigned);
    assert_ne!(task.assignee.as_deref(), Some("dead-worker"));
    assert!(
        gateway
            .calls()
            .iter()
            .any(|call| call.method == "send_prompt"),
        "dispatch should run after reconciliation requeues the task"
    );
}

#[tokio::test]
async fn startup_reconciliation_runs_before_worker_spawn() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let run_id = RunId::new("run-startup");
    let task_id = TaskId::new("T-startup");
    let now = Utc::now();

    run_store
        .create_run(RunRecord::new(
            run_id.clone(),
            task_id,
            RunRole::Worker,
            now - ChronoDuration::minutes(10),
        ))
        .await
        .unwrap();
    run_store
        .claim_run(ClaimRequest::new(
            run_id.clone(),
            ClaimOwner::new("old-worker"),
            Some(SessionId::new("old-session")),
            now - ChronoDuration::minutes(9),
            now - ChronoDuration::minutes(5),
        ))
        .await
        .unwrap();

    let gateway = Arc::new(AssertReleasedBeforeSpawnGateway::new(
        run_store.clone(),
        run_id.clone(),
    ));

    let mut config = OrchestratorConfig::default();
    config.worker_config.min_count = 1;
    config.poll_interval = std::time::Duration::from_millis(1);

    let deps = OrchestratorDeps {
        event_store,
        gateway: gateway.clone(),
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.set_run_store(run_store.clone());

    orchestrator
        .run(Arc::new(AtomicBool::new(true)))
        .await
        .unwrap();

    let record = run_store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Released);
    assert!(
        gateway.spawn_checked.load(Ordering::SeqCst),
        "gateway spawn must observe startup-reconciled run state"
    );
}

#[tokio::test]
async fn retry_queued_not_due_is_ignored_by_reconciliation() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let task_id = TaskId::new("T-retry-wait");
    let now = Utc::now();

    let mut queued = RunRecord::new(
        RunId::new("run-retry-wait"),
        task_id.clone(),
        RunRole::Worker,
        now - ChronoDuration::minutes(5),
    );
    queued.status = RunStatus::RetryQueued;
    queued.attempt = 1;
    queued.retry_queued_at = Some(now - ChronoDuration::minutes(1));
    queued.retry_at = Some(now + ChronoDuration::minutes(5));
    queued.retry_reason = Some("retry later".into());
    run_store.create_run(queued.clone()).await.unwrap();

    let mut config = OrchestratorConfig::default();
    config.worktree_isolation = false;
    let deps = OrchestratorDeps {
        event_store,
        gateway: Arc::new(MockGateway::new()),
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.add_task(task_id.clone(), "retry wait".into(), "desc".into(), vec![]);
    orchestrator.set_run_store(run_store.clone());

    let report = orchestrator
        .reconcile_before_dispatch("test")
        .await
        .unwrap();

    let records = run_store.runs_for_task(&task_id).await.unwrap();
    assert_eq!(report.run_retry_attempts, 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, RunStatus::RetryQueued);
    assert_eq!(records[0].retry_at, queued.retry_at);
}

#[tokio::test]
async fn dispatch_skips_task_with_retry_queued_not_due() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let gateway = Arc::new(MockGateway::new());
    let task_id = TaskId::new("T-dispatch-wait");
    let now = Utc::now();

    let mut queued = RunRecord::new(
        RunId::new("run-dispatch-wait"),
        task_id.clone(),
        RunRole::Worker,
        now - ChronoDuration::minutes(5),
    );
    queued.status = RunStatus::RetryQueued;
    queued.retry_queued_at = Some(now - ChronoDuration::minutes(1));
    queued.retry_at = Some(now + ChronoDuration::minutes(5));
    queued.retry_reason = Some("retry later".into());
    run_store.create_run(queued).await.unwrap();

    let mut config = OrchestratorConfig::default();
    config.worktree_isolation = false;
    let deps = OrchestratorDeps {
        event_store,
        gateway: gateway.clone(),
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.spawn_workers_to_min().await.unwrap();
    orchestrator.add_task(
        task_id.clone(),
        "dispatch wait".into(),
        "desc".into(),
        vec![],
    );
    orchestrator.set_run_store(run_store);

    orchestrator.tick().await.unwrap();

    let task = orchestrator.task_board().get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Pending);
    assert!(
        gateway
            .calls()
            .iter()
            .all(|call| call.method != "send_prompt"),
        "dispatch must wait for retry_at before sending another prompt"
    );
}

#[tokio::test]
async fn retry_queued_due_creates_new_attempt() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let task_id = TaskId::new("T-retry-due");
    let now = Utc::now();

    let mut queued = RunRecord::new(
        RunId::new("run-retry-due"),
        task_id.clone(),
        RunRole::Worker,
        now - ChronoDuration::minutes(5),
    );
    queued.status = RunStatus::RetryQueued;
    queued.attempt = 1;
    queued.retry_queued_at = Some(now - ChronoDuration::minutes(1));
    queued.retry_at = Some(now - ChronoDuration::seconds(1));
    queued.retry_reason = Some("retry now".into());
    run_store.create_run(queued.clone()).await.unwrap();

    let mut config = OrchestratorConfig::default();
    config.worktree_isolation = false;
    let deps = OrchestratorDeps {
        event_store,
        gateway: Arc::new(MockGateway::new()),
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.add_task(task_id.clone(), "retry due".into(), "desc".into(), vec![]);
    orchestrator.set_run_store(run_store.clone());

    let report = orchestrator
        .reconcile_before_dispatch("test")
        .await
        .unwrap();

    let records = run_store.runs_for_task(&task_id).await.unwrap();
    assert_eq!(report.run_retry_attempts, 1);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].status, RunStatus::Failed);
    assert_eq!(records[1].status, RunStatus::Created);
    assert_eq!(records[1].attempt, 2);
    assert_eq!(records[1].task_id, task_id);
    assert_eq!(records[1].role, RunRole::Worker);
    assert_ne!(records[0].run_id, records[1].run_id);
}

#[tokio::test]
async fn failed_retryable_run_moves_to_retry_queued() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let task_id = TaskId::new("T-retry-queue");
    let now = Utc::now();

    let mut running = RunRecord::new(
        RunId::new("run-retry-queue"),
        task_id.clone(),
        RunRole::Worker,
        now - ChronoDuration::minutes(5),
    );
    running.status = RunStatus::Running;
    running.attempt = 1;
    running.claim_generation = brehon_types::ClaimGeneration::new(1);
    running.claim_owner = Some(ClaimOwner::new("dead-worker"));
    running.session_id = Some(SessionId::new("dead-session"));
    running.lease_expires_at = Some(now + ChronoDuration::minutes(5));
    running.claimed_at = Some(now - ChronoDuration::minutes(4));
    run_store.create_run(running.clone()).await.unwrap();

    let mut config = OrchestratorConfig::default();
    config.worktree_isolation = false;
    let deps = OrchestratorDeps {
        event_store,
        gateway: Arc::new(MockGateway::new()),
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.add_task(task_id.clone(), "retry queue".into(), "desc".into(), vec![]);
    orchestrator
        .task_board
        .update_task_status(&task_id, TaskStatus::InProgress)
        .unwrap();
    orchestrator
        .task_board
        .assign_task(&task_id, "dead-worker", Some("dead-session"))
        .unwrap();
    orchestrator.set_run_store(run_store.clone());

    let report = orchestrator
        .reconcile_before_dispatch("test")
        .await
        .unwrap();

    let record = run_store.get_run(&running.run_id).await.unwrap().unwrap();
    assert_eq!(report.run_retries_queued, 1);
    assert_eq!(record.status, RunStatus::RetryQueued);
    assert!(record.retry_at.is_some());
    assert!(record.retry_queued_at.is_some());
    assert_eq!(
        record.retry_reason.as_deref(),
        Some("dead or missing session: interrupted run")
    );
    assert!(record.claim_owner.is_none());
}

async fn append_task_event(store: &InMemoryEventStore, kind: EventKind) {
    store
        .append(Event {
            kind,
            timestamp: Utc::now(),
            aggregate_id: "T001".to_string(),
        })
        .await
        .unwrap();
}

#[derive(Debug)]
struct AssertReleasedBeforeSpawnGateway {
    inner: MockGateway,
    run_store: Arc<InMemoryRunStore>,
    run_id: RunId,
    spawn_checked: AtomicBool,
}

impl AssertReleasedBeforeSpawnGateway {
    fn new(run_store: Arc<InMemoryRunStore>, run_id: RunId) -> Self {
        Self {
            inner: MockGateway::new(),
            run_store,
            run_id,
            spawn_checked: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl AgentGateway for AssertReleasedBeforeSpawnGateway {
    async fn spawn(&self, spec: SessionSpec) -> Result<SessionId, PortError> {
        let record = self
            .run_store
            .get_run(&self.run_id)
            .await
            .map_err(|err| PortError::Storage(err.to_string()))?
            .ok_or_else(|| PortError::Storage("startup run missing".to_string()))?;
        assert_eq!(record.status, RunStatus::Released);
        self.spawn_checked.store(true, Ordering::SeqCst);
        self.inner.spawn(spec).await
    }

    async fn set_config(
        &self,
        session: &SessionId,
        option: &str,
        value: &str,
    ) -> Result<(), PortError> {
        self.inner.set_config(session, option, value).await
    }

    async fn send_prompt(
        &self,
        session: &SessionId,
        prompt: PromptTurn,
    ) -> Result<PromptHandle, PortError> {
        self.inner.send_prompt(session, prompt).await
    }

    async fn cancel_prompt(&self, session: &SessionId, prompt: &PromptId) -> Result<(), PortError> {
        self.inner.cancel_prompt(session, prompt).await
    }

    async fn attach_terminal(&self, session: &SessionId) -> Result<Option<TerminalId>, PortError> {
        self.inner.attach_terminal(session).await
    }

    async fn send_terminal_input(
        &self,
        terminal: &TerminalId,
        input: Vec<u8>,
    ) -> Result<(), PortError> {
        self.inner.send_terminal_input(terminal, input).await
    }

    async fn resolve_permission(
        &self,
        session: &SessionId,
        permission_id: &str,
        approved: bool,
    ) -> Result<(), PortError> {
        self.inner
            .resolve_permission(session, permission_id, approved)
            .await
    }

    async fn kill_session(&self, session: &SessionId) -> Result<(), PortError> {
        self.inner.kill_session(session).await
    }

    async fn health_check(&self, session: &SessionId) -> Result<HealthStatus, PortError> {
        self.inner.health_check(session).await
    }

    async fn capabilities(&self, session: &SessionId) -> Result<AgentCapabilities, PortError> {
        self.inner.capabilities(session).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, PortError> {
        self.inner.list_sessions().await
    }
}
