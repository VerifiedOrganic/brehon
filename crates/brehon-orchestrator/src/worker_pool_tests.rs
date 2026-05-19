use std::sync::Arc;

use brehon_test_harness::MockGateway;
use brehon_types::{TaskId, TaskStatus};

use crate::error::OrchestratorError;
use crate::task_board::{TaskBoard, TaskEntry};
use crate::worker_pool::{parse_worker_overrides, WorkerKind, WorkerPool, WorkerPoolConfig};

#[test]
fn parse_worker_override_empty() {
    let result = parse_worker_overrides("").unwrap();
    assert!(result.is_empty());
}

#[test]
fn parse_worker_override_single() {
    let result = parse_worker_overrides("claude-code:3").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].agent_type, "claude-code");
    assert_eq!(result[0].count, 3);
    assert_eq!(result[0].kind, WorkerKind::Worker);
}

#[test]
fn parse_worker_override_multiple() {
    let result = parse_worker_overrides("claude-code:3,codex:2").unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].agent_type, "claude-code");
    assert_eq!(result[0].count, 3);
    assert_eq!(result[1].agent_type, "codex");
    assert_eq!(result[1].count, 2);
}

#[test]
fn parse_worker_override_invalid_format() {
    let result = parse_worker_overrides("claude-code");
    assert!(result.is_err());
}

#[test]
fn parse_worker_override_invalid_count() {
    let result = parse_worker_overrides("claude-code:abc");
    assert!(result.is_err());
}

#[tokio::test]
async fn worker_pool_initial_state() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig::default();
    let pool = WorkerPool::new(config, gateway);

    assert_eq!(pool.alive_count(), 0);
    assert_eq!(pool.total_count(), 0);
    assert_eq!(pool.available_count(), 0);
}

#[tokio::test]
async fn spawn_to_min_creates_workers() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig {
        min_count: 2,
        max_count: 5,
        ..Default::default()
    };
    let mut pool = WorkerPool::new(config, gateway);

    let spawned = pool.spawn_to_min().await.unwrap();
    assert_eq!(spawned.len(), 2);
    assert_eq!(pool.alive_count(), 2);
}

#[tokio::test]
async fn spawn_to_min_no_spawns_when_at_min() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig {
        min_count: 1,
        max_count: 5,
        ..Default::default()
    };
    let mut pool = WorkerPool::new(config, gateway);

    pool.spawn_to_min().await.unwrap();
    assert_eq!(pool.alive_count(), 1);

    let spawned = pool.spawn_to_min().await.unwrap();
    assert!(spawned.is_empty());
}

#[tokio::test]
async fn assign_task_to_worker() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::new(config, gateway);

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let info = pool.get_worker(&worker).unwrap();
    assert_eq!(info.assigned_task, Some("T001".to_string()));

    let worker2 = pool.get_idle_worker();
    assert!(worker2.is_none());
}

#[tokio::test]
async fn clear_task_assignment() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::new(config, gateway);

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    pool.clear_assignment(&worker).unwrap();

    let info = pool.get_worker(&worker).unwrap();
    assert!(info.assigned_task.is_none());
    assert!(pool.get_idle_worker().is_some());
}

#[tokio::test]
async fn assignment_history() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::new(config, gateway);

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let history = pool.assignment_history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].worker_id, worker);
}

#[tokio::test]
async fn assign_rejects_when_worker_has_in_review_task() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::InReview;
    task_board.add_task(task);

    let result = pool.assign_task(&worker, "T002");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, OrchestratorError::AssignmentError(_)));
    if let OrchestratorError::AssignmentError(msg) = err {
        assert!(msg.contains("already has non-terminal task"));
        assert!(msg.contains("T001"));
        assert!(msg.contains("InReview"));
    }
}

#[tokio::test]
async fn assign_rejects_when_worker_has_changes_requested_task() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::ChangesRequested;
    task_board.add_task(task);

    let result = pool.assign_task(&worker, "T002");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, OrchestratorError::AssignmentError(_)));
    if let OrchestratorError::AssignmentError(msg) = err {
        assert!(msg.contains("already has non-terminal task"));
        assert!(msg.contains("T001"));
        assert!(msg.contains("ChangesRequested"));
    }
}

#[tokio::test]
async fn assign_rejects_when_worker_has_assigned_task() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::Assigned;
    task_board.add_task(task);

    let result = pool.assign_task(&worker, "T002");
    assert!(result.is_err());
}

#[tokio::test]
async fn assign_rejects_when_worker_has_approved_task() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::Approved;
    task_board.add_task(task);

    let result = pool.assign_task(&worker, "T002");
    assert!(result.is_err());
}

#[tokio::test]
async fn assign_rejects_when_worker_has_blocked_task() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::Blocked;
    task_board.add_task(task);

    let result = pool.assign_task(&worker, "T002");
    assert!(
        result.is_err(),
        "Blocked is NOT terminal - worker still owns task"
    );
    if let OrchestratorError::AssignmentError(msg) = result.unwrap_err() {
        assert!(msg.contains("already has non-terminal task"));
    }
}

#[tokio::test]
async fn assign_succeeds_after_task_reaches_terminal() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig::default();
    let task_board = Arc::new(TaskBoard::new());
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::Merged;
    task_board.add_task(task);

    let result = pool.assign_task(&worker, "T002");
    assert!(result.is_ok());
}

#[tokio::test]
async fn get_idle_workers_excludes_in_review() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig {
        min_count: 2,
        max_count: 5,
        ..Default::default()
    };
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let workers = pool.get_idle_workers();
    assert_eq!(workers.len(), 2);

    let worker = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::InReview;
    task_board.add_task(task);

    let idle_workers = pool.get_idle_workers();
    assert_eq!(idle_workers.len(), 1);
    assert!(!idle_workers.contains(&worker));
}

#[tokio::test]
async fn get_idle_workers_includes_worker_with_terminal_task() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig {
        min_count: 2,
        max_count: 5,
        ..Default::default()
    };
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board.clone());

    pool.spawn_to_min().await.unwrap();

    let worker1 = pool.get_idle_worker().unwrap();
    pool.assign_task(&worker1, "T001").unwrap();

    let mut task = TaskEntry::new(
        TaskId::new("T001"),
        "Test".to_string(),
        "Description".to_string(),
    );
    task.status = TaskStatus::Merged;
    task_board.add_task(task);

    let idle_workers = pool.get_idle_workers();
    assert_eq!(idle_workers.len(), 2);
    assert!(idle_workers.contains(&worker1));
}

#[tokio::test]
async fn handle_worker_death_clears_dead_worker_assignment() {
    let gateway = Arc::new(MockGateway::new());
    let task_board = Arc::new(TaskBoard::new());
    let config = WorkerPoolConfig {
        min_count: 0,
        max_count: 3,
        ..Default::default()
    };
    let mut pool = WorkerPool::with_task_board(config, gateway, task_board);
    let worker = pool.spawn_worker(0).await.unwrap();

    pool.assign_task(&worker, "T001").unwrap();

    let dead_session = pool.get_worker(&worker).unwrap().session_id.clone();
    let replacement = pool.handle_worker_death(&dead_session).await.unwrap();

    let dead_worker = pool.get_worker(&worker).unwrap();
    assert!(
        dead_worker.assigned_task.is_none(),
        "dead worker assignment should be cleared for reconciliation"
    );
    assert!(
        !dead_worker.is_alive,
        "dead worker record should remain marked dead when min_count does not require respawn"
    );
    assert!(
        replacement.is_none(),
        "min_count=0 should not trigger replacement spawn during death handling"
    );
}

#[tokio::test]
async fn assignment_history_is_bounded() {
    let gateway = Arc::new(MockGateway::new());
    let config = WorkerPoolConfig::default();
    let mut pool = WorkerPool::new(config, gateway);
    pool.set_max_assignment_history(3);
    pool.spawn_to_min().await.unwrap();

    let worker = pool.get_idle_worker().unwrap();
    for i in 0..5 {
        pool.clear_assignment(&worker).ok();
        pool.assign_task(&worker, &format!("T{}", i)).unwrap();
    }

    assert_eq!(pool.assignment_history().len(), 3);
}
