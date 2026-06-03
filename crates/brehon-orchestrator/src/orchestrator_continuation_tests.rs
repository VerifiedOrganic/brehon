use std::sync::Arc;

use brehon_ports::{AgentGateway, RunStore};
use brehon_test_harness::{InMemoryEventStore, InMemoryRunStore, MockBehavior, MockGateway};
use brehon_types::{
    AgentId, ClaimGeneration, ClaimOwner, EventKind, RunId, RunRecord, RunRole, RunStatus,
    SessionId, SessionSpec, TaskId, TaskStatus,
};
use chrono::{Duration as ChronoDuration, Utc};

use crate::orchestrator::{Orchestrator, OrchestratorConfig, OrchestratorDeps};

#[tokio::test]
async fn active_continuation_prompts_same_run_without_creating_new_run() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let gateway = Arc::new(MockGateway::new());
    let task_id = TaskId::new("T-continuation-active");
    let run_id = RunId::new("run-continuation-active");
    let session_id = SessionId::new("session-continuation-active");

    let mut orchestrator =
        continuation_orchestrator(event_store.clone(), run_store.clone(), gateway.clone(), 2);
    add_live_task_and_session(
        &mut orchestrator,
        &gateway,
        &task_id,
        &session_id,
        "worker-1",
    );
    create_active_run(
        run_store.as_ref(),
        run_id.clone(),
        task_id.clone(),
        session_id.clone(),
        0,
    )
    .await;

    let report = orchestrator.continue_active_runs().await.unwrap();

    assert_eq!(report.continued, 1);
    assert_eq!(report.escalated, 0);
    assert_eq!(report.events_emitted, 1);
    assert_eq!(call_count(&gateway, "send_prompt"), 1);
    assert!(
        gateway.calls().iter().all(|call| call.method != "spawn"),
        "continuation must reuse the existing session"
    );

    let runs = run_store.runs_for_task(&task_id).await.unwrap();
    assert_eq!(runs.len(), 1, "continuation must not create another run");
    assert_eq!(runs[0].run_id, run_id);
    assert_eq!(runs[0].continuation_turns, 1);
    assert!(runs[0].last_continuation_at.is_some());

    let events = event_store.all_events();
    assert!(events
        .iter()
        .any(|event| matches!(event.kind, EventKind::RunActivityObserved { .. })));
}

#[tokio::test]
async fn unhealthy_session_escalates_without_prompting() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let gateway = Arc::new(MockGateway::new());
    let task_id = TaskId::new("T-continuation-unhealthy");
    let run_id = RunId::new("run-continuation-unhealthy");
    let session_id = SessionId::new("session-continuation-unhealthy");

    let mut orchestrator =
        continuation_orchestrator(event_store.clone(), run_store.clone(), gateway.clone(), 2);
    add_live_task_and_session(
        &mut orchestrator,
        &gateway,
        &task_id,
        &session_id,
        "worker-1",
    );
    gateway.kill_session(&session_id).await.unwrap();
    create_active_run(
        run_store.as_ref(),
        run_id.clone(),
        task_id.clone(),
        session_id.clone(),
        0,
    )
    .await;

    let report = orchestrator.continue_active_runs().await.unwrap();

    assert_eq!(report.continued, 0);
    assert_eq!(report.escalated, 1);
    assert_eq!(report.events_emitted, 1);
    assert_eq!(call_count(&gateway, "send_prompt"), 0);

    let run = run_store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(run.continuation_turns, 0);
    assert!(event_store
        .all_events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::EscalationTriggered { .. })));
}

#[tokio::test]
async fn prompt_send_failure_escalates_without_recording_continuation() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let gateway = Arc::new(MockGateway::new());
    let task_id = TaskId::new("T-continuation-send-failure");
    let run_id = RunId::new("run-continuation-send-failure");
    let session_id = SessionId::new("session-continuation-send-failure");

    let mut orchestrator =
        continuation_orchestrator(event_store.clone(), run_store.clone(), gateway.clone(), 2);
    add_live_task_and_session_with_behavior(
        &mut orchestrator,
        &gateway,
        &task_id,
        &session_id,
        "worker-1",
        MockBehavior::crashing_after(1),
    );
    create_active_run(
        run_store.as_ref(),
        run_id.clone(),
        task_id.clone(),
        session_id.clone(),
        0,
    )
    .await;

    let report = orchestrator.continue_active_runs().await.unwrap();

    assert_eq!(report.continued, 0);
    assert_eq!(report.escalated, 1);
    assert_eq!(report.events_emitted, 1);
    assert_eq!(call_count(&gateway, "send_prompt"), 1);

    let run = run_store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(run.continuation_turns, 0);
    assert!(run.last_continuation_at.is_none());
    assert!(event_store
        .all_events()
        .iter()
        .any(|event| matches!(event.kind, EventKind::EscalationTriggered { .. })));
}

#[tokio::test]
async fn max_turn_cap_stops_without_prompting() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let gateway = Arc::new(MockGateway::new());
    let task_id = TaskId::new("T-continuation-cap");
    let run_id = RunId::new("run-continuation-cap");
    let session_id = SessionId::new("session-continuation-cap");

    let mut orchestrator =
        continuation_orchestrator(event_store.clone(), run_store.clone(), gateway.clone(), 1);
    add_live_task_and_session(
        &mut orchestrator,
        &gateway,
        &task_id,
        &session_id,
        "worker-1",
    );
    create_active_run(
        run_store.as_ref(),
        run_id.clone(),
        task_id.clone(),
        session_id.clone(),
        1,
    )
    .await;

    let report = orchestrator.continue_active_runs().await.unwrap();

    assert_eq!(report.continued, 0);
    assert_eq!(report.stopped, 1);
    assert_eq!(report.events_emitted, 0);
    assert_eq!(call_count(&gateway, "send_prompt"), 0);

    let run = run_store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(run.continuation_turns, 1);
    assert!(run.last_continuation_at.is_none());
}

fn continuation_orchestrator(
    event_store: Arc<InMemoryEventStore>,
    run_store: Arc<InMemoryRunStore>,
    gateway: Arc<MockGateway>,
    max_turns_per_run: u32,
) -> Orchestrator {
    let mut config = OrchestratorConfig {
        worktree_isolation: false,
        ..OrchestratorConfig::default()
    };
    config.continuation_policy.idle_prompt_after_secs = 1;
    config.continuation_policy.max_turns_per_run = max_turns_per_run;

    let deps = OrchestratorDeps {
        event_store,
        gateway,
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.set_run_store(run_store);
    orchestrator
}

fn add_live_task_and_session(
    orchestrator: &mut Orchestrator,
    gateway: &MockGateway,
    task_id: &TaskId,
    session_id: &SessionId,
    owner: &str,
) {
    add_live_task_and_session_with_behavior(
        orchestrator,
        gateway,
        task_id,
        session_id,
        owner,
        Default::default(),
    );
}

fn add_live_task_and_session_with_behavior(
    orchestrator: &mut Orchestrator,
    gateway: &MockGateway,
    task_id: &TaskId,
    session_id: &SessionId,
    owner: &str,
    behavior: MockBehavior,
) {
    gateway.add_session_with_behavior(
        session_id.as_str(),
        SessionSpec::new(
            AgentId::new(owner),
            "worker".to_string(),
            "/tmp/brehon-continuation-test".to_string(),
        ),
        behavior,
        vec![],
    );
    orchestrator.add_task(
        task_id.clone(),
        "continuation task".into(),
        "description".into(),
        vec![],
    );
    orchestrator
        .task_board()
        .assign_task(task_id, owner, Some(session_id.as_str()))
        .unwrap();
    orchestrator
        .task_board()
        .update_task_status(task_id, TaskStatus::InProgress)
        .unwrap();
}

async fn create_active_run(
    run_store: &dyn RunStore,
    run_id: RunId,
    task_id: TaskId,
    session_id: SessionId,
    continuation_turns: u32,
) {
    let now = Utc::now();
    let mut run = RunRecord::new(
        run_id,
        task_id,
        RunRole::Worker,
        now - ChronoDuration::minutes(20),
    );
    run.status = RunStatus::Running;
    run.claim_generation = ClaimGeneration::new(1);
    run.claim_owner = Some(ClaimOwner::new("worker-1"));
    run.session_id = Some(session_id);
    run.lease_expires_at = Some(now + ChronoDuration::minutes(10));
    run.claimed_at = Some(now - ChronoDuration::minutes(19));
    run.started_at = Some(now - ChronoDuration::minutes(18));
    run.last_activity_at = Some(now - ChronoDuration::minutes(5));
    run.continuation_turns = continuation_turns;

    run_store.create_run(run).await.unwrap();
}

fn call_count(gateway: &MockGateway, method: &str) -> usize {
    gateway
        .calls()
        .iter()
        .filter(|call| call.method == method)
        .count()
}
