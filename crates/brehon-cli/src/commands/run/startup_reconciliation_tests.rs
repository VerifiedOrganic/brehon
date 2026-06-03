use std::sync::Arc;

use chrono::{Duration as ChronoDuration, Utc};

use brehon_orchestrator::{Orchestrator, OrchestratorConfig, OrchestratorDeps};
use brehon_ports::{ClaimRequest, RunStore};
use brehon_test_harness::{InMemoryEventStore, InMemoryRunStore, MockGateway};
use brehon_types::{ClaimOwner, RunId, RunRecord, RunRole, RunStatus, SessionId, TaskId};

#[tokio::test]
async fn startup_reconciliation_orchestrator_api_releases_expired_claim() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let run_store = Arc::new(InMemoryRunStore::new());
    let now = Utc::now();
    let run_id = RunId::new("cli-startup-run");

    run_store
        .create_run(RunRecord::new(
            run_id.clone(),
            TaskId::new("T-cli-startup"),
            RunRole::Worker,
            now - ChronoDuration::minutes(10),
        ))
        .await
        .unwrap();
    run_store
        .claim_run(ClaimRequest::new(
            run_id.clone(),
            ClaimOwner::new("stale-worker"),
            Some(SessionId::new("stale-session")),
            now - ChronoDuration::minutes(9),
            now - ChronoDuration::minutes(5),
        ))
        .await
        .unwrap();

    let mut config = OrchestratorConfig::default();
    config.worker_config.min_count = 0;

    let deps = OrchestratorDeps {
        event_store,
        gateway: Arc::new(MockGateway::new()),
        git_ops: None,
        decision_engine: None,
    };
    let mut orchestrator = Orchestrator::new(config, deps);
    orchestrator.set_run_store(run_store.clone());

    orchestrator.run_startup_reconciliation().await.unwrap();

    let record = run_store.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Released);
    assert_eq!(record.release_reason.as_deref(), Some("expired claim"));
}
