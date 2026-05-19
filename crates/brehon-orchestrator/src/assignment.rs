//! Task-to-worker assignment logic.
//!
//! Handles assignment dispatch, round-robin for simple cases,
//! and DecisionEngine delegation for complex assignments.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tracing::debug;

use brehon_ports::{AgentGateway, DecisionEngine};
use brehon_types::{MessageKind, PromptId, PromptTurn, SessionId, TaskId};

use crate::error::{OrchestratorError, Result};
use crate::task_board::TaskEntry;
use crate::worker_pool::{WorkerId, WorkerInfo, WorkerPool};

#[derive(Debug, Clone)]
pub struct Assignment {
    pub task_id: TaskId,
    pub worker_id: WorkerId,
    pub session_id: SessionId,
    pub assigned_at: chrono::DateTime<chrono::Utc>,
}

pub struct AssignmentEngine {
    worker_pool: Arc<parking_lot::RwLock<WorkerPool>>,
    gateway: Arc<dyn AgentGateway>,
    #[allow(dead_code)]
    decision_engine: Option<Arc<dyn DecisionEngine>>,
    round_robin_indices: parking_lot::RwLock<HashMap<String, usize>>,
    assignment_history: parking_lot::RwLock<Vec<Assignment>>,
}

impl AssignmentEngine {
    pub fn new(
        worker_pool: Arc<parking_lot::RwLock<WorkerPool>>,
        gateway: Arc<dyn AgentGateway>,
        decision_engine: Option<Arc<dyn DecisionEngine>>,
    ) -> Self {
        Self {
            worker_pool,
            gateway,
            decision_engine,
            round_robin_indices: parking_lot::RwLock::new(HashMap::new()),
            assignment_history: parking_lot::RwLock::new(Vec::new()),
        }
    }

    pub async fn assign_task(&self, task: &TaskEntry) -> Result<Assignment> {
        let worker_id = self.select_worker_for_task(task)?;

        let pool = self.worker_pool.read();
        let worker = pool.get_worker(&worker_id).ok_or_else(|| {
            OrchestratorError::AssignmentError(format!("Worker {} not found", worker_id))
        })?;

        let session_id = worker.session_id.clone();
        drop(pool);

        let assignment = Assignment {
            task_id: task.id.clone(),
            worker_id: worker_id.clone(),
            session_id: session_id.clone(),
            assigned_at: chrono::Utc::now(),
        };

        self.worker_pool
            .write()
            .assign_task(&worker_id, task.id.as_str())?;

        self.assignment_history.write().push(assignment.clone());

        debug!(
            task_id = %task.id,
            worker_id = %worker_id,
            session_id = %session_id,
            "Task assigned"
        );

        Ok(assignment)
    }

    fn select_worker_for_task(&self, _task: &TaskEntry) -> Result<WorkerId> {
        let pool = self.worker_pool.read();

        let idle_workers: Vec<_> = pool
            .alive_workers()
            .filter(|w| w.assigned_task.is_none())
            .collect();

        if idle_workers.is_empty() {
            return Err(OrchestratorError::NoAvailableWorkers);
        }

        if idle_workers.len() == 1 {
            return Ok(idle_workers[0].id.clone());
        }

        let worker_id = self.round_robin_select(&idle_workers);
        drop(pool);

        Ok(worker_id)
    }

    fn round_robin_select(&self, workers: &[&WorkerInfo]) -> WorkerId {
        let agent_type = workers[0].agent_type.clone();

        let mut indices = self.round_robin_indices.write();
        let index = indices.entry(agent_type.clone()).or_insert(0);

        let selected = workers[*index % workers.len()];
        *index = (*index + 1) % workers.len();

        selected.id.clone()
    }

    pub async fn dispatch_task(&self, assignment: &Assignment, task: &TaskEntry) -> Result<()> {
        let prompt = build_task_prompt(task);

        self.gateway
            .send_prompt(&assignment.session_id, prompt)
            .await
            .map_err(|e| {
                OrchestratorError::AssignmentError(format!("Failed to dispatch task: {}", e))
            })?;

        debug!(
            task_id = %task.id,
            worker_id = %assignment.worker_id,
            "Task dispatched"
        );

        Ok(())
    }

    pub fn handle_worker_death(&self, worker_id: &WorkerId) -> Result<Vec<TaskId>> {
        let history = self.assignment_history.read();
        let affected_tasks: Vec<TaskId> = history
            .iter()
            .filter_map(|a| {
                if a.worker_id == *worker_id {
                    Some(a.task_id.clone())
                } else {
                    None
                }
            })
            .collect();

        debug!(
            worker_id = %worker_id,
            affected_tasks = affected_tasks.len(),
            "Worker death handled"
        );

        Ok(affected_tasks)
    }

    pub fn reassign_task(&self, task: &TaskEntry) -> Result<Assignment> {
        let worker_id = self.select_worker_for_task(task)?;

        let pool = self.worker_pool.read();
        let worker = pool.get_worker(&worker_id).ok_or_else(|| {
            OrchestratorError::AssignmentError(format!("Worker {} not found", worker_id))
        })?;

        let session_id = worker.session_id.clone();
        drop(pool);

        let assignment = Assignment {
            task_id: task.id.clone(),
            worker_id: worker_id.clone(),
            session_id: session_id.clone(),
            assigned_at: chrono::Utc::now(),
        };

        self.worker_pool
            .write()
            .assign_task(&worker_id, task.id.as_str())?;

        self.assignment_history.write().push(assignment.clone());

        debug!(
            task_id = %task.id,
            new_worker_id = %worker_id,
            "Task reassigned"
        );

        Ok(assignment)
    }

    pub fn get_assignment_for_task(&self, task_id: &TaskId) -> Option<Assignment> {
        self.assignment_history
            .read()
            .iter()
            .rev()
            .find(|a| a.task_id == *task_id)
            .cloned()
    }

    pub fn get_assignments_for_worker(&self, worker_id: &WorkerId) -> Vec<Assignment> {
        self.assignment_history
            .read()
            .iter()
            .filter(|a| a.worker_id == *worker_id)
            .cloned()
            .collect()
    }

    pub fn assignment_history(&self) -> Vec<Assignment> {
        self.assignment_history.read().clone()
    }

    /// Number of assignment records stored (including reassignments).
    ///
    /// Cheaper than `assignment_history()` when only the count is needed.
    pub fn history_len(&self) -> usize {
        self.assignment_history.read().len()
    }

    pub fn clear_completed(&self, completed_tasks: &[TaskId]) {
        let completed_set: std::collections::HashSet<_> = completed_tasks.iter().collect();

        let history = self.assignment_history.read();
        for assignment in history.iter() {
            if completed_set.contains(&assignment.task_id) {
                self.worker_pool
                    .write()
                    .clear_assignment(&assignment.worker_id)
                    .ok();
            }
        }
    }
}

fn build_task_prompt(task: &TaskEntry) -> PromptTurn {
    let content = format!(
        "Task: {}\n\nDescription: {}\n\nPriority: {:?}",
        task.title, task.description, task.priority
    );

    PromptTurn {
        prompt_id: PromptId::new(uuid::Uuid::new_v4().to_string()),
        content,
        kind: MessageKind::TaskAssignment,
        sent_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_board::TaskEntry;
    use crate::worker_pool::{WorkerKind, WorkerPoolConfig};
    use brehon_test_harness::MockGateway;
    use brehon_types::TaskId;

    #[tokio::test]
    async fn assign_task_round_robin() {
        let gateway = Arc::new(MockGateway::new());

        let config = WorkerPoolConfig {
            min_count: 2,
            max_count: 5,
            agent_type: "test".to_string(),
            kind: WorkerKind::Worker,
            worktree_base: "/tmp/test".to_string(),
            ..Default::default()
        };

        let mut initial_pool = WorkerPool::new(config, gateway.clone());
        initial_pool.spawn_to_min().await.unwrap();
        let pool = Arc::new(parking_lot::RwLock::new(initial_pool));

        let engine = AssignmentEngine::new(pool.clone(), gateway, None);

        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".into(),
            "Test description".into(),
        );

        let assignment = engine.assign_task(&task).await.unwrap();

        assert_eq!(assignment.task_id, TaskId::new("T001"));
        assert!(assignment.worker_id.as_str().starts_with("test-"));
    }

    #[tokio::test]
    async fn assign_task_no_workers() {
        let gateway = Arc::new(MockGateway::new());

        let config = WorkerPoolConfig {
            min_count: 0,
            max_count: 5,
            agent_type: "test".to_string(),
            kind: WorkerKind::Worker,
            worktree_base: "/tmp/test".to_string(),
            ..Default::default()
        };

        let pool = Arc::new(parking_lot::RwLock::new(WorkerPool::new(
            config,
            gateway.clone(),
        )));

        let engine = AssignmentEngine::new(pool.clone(), gateway, None);

        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".into(),
            "Test description".into(),
        );

        let result = engine.assign_task(&task).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OrchestratorError::NoAvailableWorkers
        ));
    }

    #[tokio::test]
    async fn round_robin_distribution() {
        let gateway = Arc::new(MockGateway::new());

        let config = WorkerPoolConfig {
            min_count: 3,
            max_count: 5,
            agent_type: "test".to_string(),
            kind: WorkerKind::Worker,
            worktree_base: "/tmp/test".to_string(),
            ..Default::default()
        };

        let mut initial_pool = WorkerPool::new(config, gateway.clone());
        initial_pool.spawn_to_min().await.unwrap();
        let pool = Arc::new(parking_lot::RwLock::new(initial_pool));

        let engine = AssignmentEngine::new(pool.clone(), gateway.clone(), None);

        let task1 = TaskEntry::new(TaskId::new("T001"), "Task 1".into(), "Desc".into());
        let task2 = TaskEntry::new(TaskId::new("T002"), "Task 2".into(), "Desc".into());
        let task3 = TaskEntry::new(TaskId::new("T003"), "Task 3".into(), "Desc".into());

        let a1 = engine.assign_task(&task1).await.unwrap();
        engine
            .worker_pool
            .write()
            .clear_assignment(&a1.worker_id)
            .unwrap();

        let a2 = engine.assign_task(&task2).await.unwrap();
        engine
            .worker_pool
            .write()
            .clear_assignment(&a2.worker_id)
            .unwrap();

        let a3 = engine.assign_task(&task3).await.unwrap();

        assert_ne!(a1.worker_id, a2.worker_id);

        assert_ne!(a2.worker_id, a3.worker_id);
    }

    #[tokio::test]
    async fn assignment_history() {
        let gateway = Arc::new(MockGateway::new());

        let config = WorkerPoolConfig {
            min_count: 1,
            max_count: 5,
            agent_type: "test".to_string(),
            kind: WorkerKind::Worker,
            worktree_base: "/tmp/test".to_string(),
            ..Default::default()
        };

        let mut initial_pool = WorkerPool::new(config, gateway.clone());
        initial_pool.spawn_to_min().await.unwrap();
        let pool = Arc::new(parking_lot::RwLock::new(initial_pool));

        let engine = AssignmentEngine::new(pool.clone(), gateway, None);

        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".into(),
            "Test description".into(),
        );

        engine.assign_task(&task).await.unwrap();

        let history = engine.assignment_history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].task_id, TaskId::new("T001"));
    }

    #[tokio::test]
    async fn get_assignment_for_task() {
        let gateway = Arc::new(MockGateway::new());

        let config = WorkerPoolConfig {
            min_count: 1,
            max_count: 5,
            agent_type: "test".to_string(),
            kind: WorkerKind::Worker,
            worktree_base: "/tmp/test".to_string(),
            ..Default::default()
        };

        let mut initial_pool = WorkerPool::new(config, gateway.clone());
        initial_pool.spawn_to_min().await.unwrap();
        let pool = Arc::new(parking_lot::RwLock::new(initial_pool));

        let engine = AssignmentEngine::new(pool.clone(), gateway, None);

        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".into(),
            "Test description".into(),
        );

        let assignment = engine.assign_task(&task).await.unwrap();

        let found = engine.get_assignment_for_task(&TaskId::new("T001"));
        assert!(found.is_some());
        assert_eq!(found.unwrap().task_id, assignment.task_id);

        let not_found = engine.get_assignment_for_task(&TaskId::new("T999"));
        assert!(not_found.is_none());
    }
}
