//! Worker pool management.
//!
//! Manages min/max counts, spawning, tracking active workers, respawn on death.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

use brehon_ports::AgentGateway;
use brehon_types::{AgentId, SessionId, SessionSpec};

use crate::error::{OrchestratorError, Result};
use crate::task_board::TaskBoard;
use crate::task_lifecycle::TaskLifecycle;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerId(pub String);

impl WorkerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerKind {
    Worker,
    Reviewer,
}

#[derive(Debug, Clone)]
pub struct WorkerInfo {
    pub id: WorkerId,
    pub kind: WorkerKind,
    pub agent_type: String,
    pub session_id: SessionId,
    pub assigned_task: Option<String>,
    pub spawned_at: DateTime<Utc>,
    pub is_alive: bool,
}

impl WorkerInfo {
    pub fn new(id: WorkerId, kind: WorkerKind, agent_type: String, session_id: SessionId) -> Self {
        Self {
            id,
            kind,
            agent_type,
            session_id,
            assigned_task: None,
            spawned_at: Utc::now(),
            is_alive: true,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerSpawnPlan {
    pub worker_id: WorkerId,
    pub spec: SessionSpec,
}

#[derive(Debug, Clone)]
pub struct WorkerPoolConfig {
    pub agent_type: String,
    pub kind: WorkerKind,
    pub min_count: u32,
    pub max_count: u32,
    pub worktree_base: String,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            agent_type: "default".to_string(),
            kind: WorkerKind::Worker,
            min_count: 1,
            max_count: 3,
            worktree_base: "/tmp/brehon/worktrees".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerAssignment {
    pub worker_id: WorkerId,
    pub session_id: SessionId,
    pub assigned_at: DateTime<Utc>,
}

pub struct WorkerPool {
    config: WorkerPoolConfig,
    workers: HashMap<WorkerId, WorkerInfo>,
    session_to_worker: HashMap<SessionId, WorkerId>,
    assignment_history: Vec<WorkerAssignment>,
    max_assignment_history: usize,
    gateway: Arc<dyn AgentGateway>,
    task_board: Option<Arc<TaskBoard>>,
}

impl WorkerPool {
    pub fn new(config: WorkerPoolConfig, gateway: Arc<dyn AgentGateway>) -> Self {
        Self::with_max_assignment_history(config, gateway, 1_000)
    }

    pub fn with_max_assignment_history(
        config: WorkerPoolConfig,
        gateway: Arc<dyn AgentGateway>,
        max_assignment_history: usize,
    ) -> Self {
        Self {
            config,
            workers: HashMap::new(),
            session_to_worker: HashMap::new(),
            assignment_history: Vec::new(),
            max_assignment_history,
            gateway,
            task_board: None,
        }
    }

    pub fn with_task_board(
        config: WorkerPoolConfig,
        gateway: Arc<dyn AgentGateway>,
        task_board: Arc<TaskBoard>,
    ) -> Self {
        Self::with_task_board_and_max_assignment_history(config, gateway, task_board, 1_000)
    }

    pub fn with_task_board_and_max_assignment_history(
        config: WorkerPoolConfig,
        gateway: Arc<dyn AgentGateway>,
        task_board: Arc<TaskBoard>,
        max_assignment_history: usize,
    ) -> Self {
        Self {
            config,
            workers: HashMap::new(),
            session_to_worker: HashMap::new(),
            assignment_history: Vec::new(),
            max_assignment_history,
            gateway,
            task_board: Some(task_board),
        }
    }

    pub fn set_task_board(&mut self, task_board: Arc<TaskBoard>) {
        self.task_board = Some(task_board);
    }

    pub fn set_max_assignment_history(&mut self, max: usize) {
        self.max_assignment_history = max;
    }

    fn is_task_terminal(&self, task_id: &str) -> bool {
        if let Some(board) = &self.task_board {
            if let Some(task) = board.get_task(&brehon_types::TaskId::new(task_id)) {
                return TaskLifecycle::is_terminal(task.status);
            }
        }
        false
    }

    fn has_non_terminal_task(&self, worker_id: &WorkerId) -> bool {
        let worker = match self.workers.get(worker_id) {
            Some(w) => w,
            None => return false,
        };

        let task_id = match &worker.assigned_task {
            Some(id) => id,
            None => return false,
        };

        if let Some(board) = &self.task_board {
            if let Some(task) = board.get_task(&brehon_types::TaskId::new(task_id)) {
                return !TaskLifecycle::is_terminal(task.status);
            }
        }

        true
    }

    pub async fn spawn_to_min(&mut self) -> Result<Vec<WorkerId>> {
        let plans = self.plan_spawn_to_min();
        if plans.is_empty() {
            let current_count = self.workers.values().filter(|w| w.is_alive).count();
            debug!(
                "Already have {} workers, min is {}",
                current_count, self.config.min_count
            );
            return Ok(Vec::new());
        }

        let mut spawned = Vec::new();
        for plan in plans {
            let worker_id = plan.worker_id.clone();
            match self.spawn_worker_from_plan(plan).await {
                Ok(worker_id) => {
                    spawned.push(worker_id);
                }
                Err(e) => {
                    warn!("Failed to spawn worker {}: {}", worker_id, e);
                }
            }
        }

        Ok(spawned)
    }

    pub(crate) fn plan_spawn_to_min(&self) -> Vec<WorkerSpawnPlan> {
        let current_count = self.workers.values().filter(|w| w.is_alive).count();
        let needed = self.config.min_count as usize;

        if current_count >= needed {
            return Vec::new();
        }

        (0..(needed - current_count))
            .map(|i| self.spawn_plan(i as u32 + current_count as u32))
            .collect()
    }

    fn spawn_plan(&self, index: u32) -> WorkerSpawnPlan {
        let worker_id = WorkerId::new(format!("{}-{}", self.config.agent_type, index));
        let worktree_path = format!(
            "{}/{}-{}",
            self.config.worktree_base, self.config.agent_type, index
        );

        let spec = SessionSpec::new(
            AgentId::new(&self.config.agent_type),
            match self.config.kind {
                WorkerKind::Worker => "worker".to_string(),
                WorkerKind::Reviewer => "reviewer".to_string(),
            },
            worktree_path,
        );

        WorkerSpawnPlan { worker_id, spec }
    }

    #[cfg(test)]
    pub(crate) async fn spawn_worker(&mut self, index: u32) -> Result<WorkerId> {
        self.spawn_worker_from_plan(self.spawn_plan(index)).await
    }

    pub(crate) fn record_spawned_worker(
        &mut self,
        plan: WorkerSpawnPlan,
        session_id: SessionId,
    ) -> WorkerId {
        let info = WorkerInfo::new(
            plan.worker_id.clone(),
            self.config.kind,
            self.config.agent_type.clone(),
            session_id.clone(),
        );

        self.workers.insert(plan.worker_id.clone(), info);
        self.session_to_worker
            .insert(session_id, plan.worker_id.clone());

        info!(
            worker_id = %plan.worker_id,
            agent_type = %self.config.agent_type,
            kind = ?self.config.kind,
            "Spawned worker"
        );

        plan.worker_id
    }

    async fn spawn_worker_from_plan(&mut self, plan: WorkerSpawnPlan) -> Result<WorkerId> {
        let session_id = self.gateway.spawn(plan.spec.clone()).await.map_err(|e| {
            OrchestratorError::WorkerPoolError(format!("Failed to spawn worker: {}", e))
        })?;
        Ok(self.record_spawned_worker(plan, session_id))
    }

    pub async fn handle_worker_death(
        &mut self,
        session_id: &SessionId,
    ) -> Result<Option<WorkerId>> {
        let spawn_plan = self.reconcile_worker_death(session_id)?;
        match spawn_plan {
            Some(plan) => self.spawn_worker_from_plan(plan).await.map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn reconcile_worker_death(
        &mut self,
        session_id: &SessionId,
    ) -> Result<Option<WorkerSpawnPlan>> {
        let worker_id = match self.session_to_worker.get(session_id) {
            Some(id) => id.clone(),
            None => {
                debug!("Unknown session died: {}", session_id);
                return Ok(None);
            }
        };

        let worker = match self.workers.get_mut(&worker_id) {
            Some(w) => w,
            None => {
                debug!("Unknown worker died: {}", worker_id);
                return Ok(None);
            }
        };

        let was_alive = worker.is_alive;
        let was_assigned_task = worker.assigned_task.is_some();
        worker.is_alive = false;
        worker.assigned_task = None;

        info!(
            worker_id = %worker_id,
            was_alive = was_alive,
            had_task = was_assigned_task,
            "Worker died"
        );

        let current_count = self.workers.values().filter(|w| w.is_alive).count();
        if current_count < self.config.min_count as usize {
            debug!("Respawning worker to maintain min count");
            return Ok(Some(
                self.spawn_plan(worker_id.as_str().parse().unwrap_or(0)),
            ));
        } else if !was_alive {
            debug!(
                worker_id = %worker_id,
                "Worker death already reconciled; no assignment to recover"
            );
        } else if !was_assigned_task {
            debug!(
                worker_id = %worker_id,
                "Worker died without active assignment; capacity already satisfied"
            );
        }

        Ok(None)
    }

    pub fn assign_task(&mut self, worker_id: &WorkerId, task_id: &str) -> Result<()> {
        if !self.workers.contains_key(worker_id) {
            return Err(OrchestratorError::WorkerNotFound(worker_id.to_string()));
        }

        let is_alive = self
            .workers
            .get(worker_id)
            .map(|w| w.is_alive)
            .unwrap_or(false);
        if !is_alive {
            return Err(OrchestratorError::AssignmentError(format!(
                "Worker {} is not alive",
                worker_id
            )));
        }

        let existing_task = self
            .workers
            .get(worker_id)
            .and_then(|w| w.assigned_task.clone());
        if let Some(ref existing) = existing_task {
            if self.has_non_terminal_task(worker_id) {
                let status_info = if let Some(board) = &self.task_board {
                    if let Some(task) = board.get_task(&brehon_types::TaskId::new(existing)) {
                        format!("{:?}", task.status)
                    } else {
                        "unknown".to_string()
                    }
                } else {
                    "unknown (no task board)".to_string()
                };
                return Err(OrchestratorError::AssignmentError(format!(
                    "Worker {} already has non-terminal task {} (status: {}). Cannot assign another.",
                    worker_id, existing, status_info
                )));
            }
        }

        let worker = self
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| OrchestratorError::WorkerNotFound(worker_id.to_string()))?;

        worker.assigned_task = Some(task_id.to_string());

        self.assignment_history.push(WorkerAssignment {
            worker_id: worker_id.clone(),
            session_id: worker.session_id.clone(),
            assigned_at: Utc::now(),
        });
        if self.assignment_history.len() > self.max_assignment_history {
            let excess = self.assignment_history.len() - self.max_assignment_history;
            self.assignment_history.drain(0..excess);
        }

        debug!(worker_id = %worker_id, task_id = %task_id, "Task assigned to worker");

        Ok(())
    }

    pub fn clear_assignment(&mut self, worker_id: &WorkerId) -> Result<()> {
        let worker = self
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| OrchestratorError::WorkerNotFound(worker_id.to_string()))?;

        worker.assigned_task = None;

        debug!(worker_id = %worker_id, "Cleared assignment");

        Ok(())
    }

    pub fn get_idle_worker(&self) -> Option<WorkerId> {
        self.workers
            .values()
            .find(|w| {
                if !w.is_alive {
                    return false;
                }
                match &w.assigned_task {
                    None => true,
                    Some(task_id) => self.is_task_terminal(task_id),
                }
            })
            .map(|w| w.id.clone())
    }

    pub fn get_idle_workers(&self) -> Vec<WorkerId> {
        self.workers
            .values()
            .filter(|w| {
                if !w.is_alive {
                    return false;
                }
                match &w.assigned_task {
                    None => true,
                    Some(task_id) => self.is_task_terminal(task_id),
                }
            })
            .map(|w| w.id.clone())
            .collect()
    }

    pub fn get_worker(&self, worker_id: &WorkerId) -> Option<&WorkerInfo> {
        self.workers.get(worker_id)
    }

    pub fn get_worker_by_session(&self, session_id: &SessionId) -> Option<&WorkerInfo> {
        self.session_to_worker
            .get(session_id)
            .and_then(|id| self.workers.get(id))
    }

    pub fn alive_count(&self) -> usize {
        self.workers.values().filter(|w| w.is_alive).count()
    }

    pub fn total_count(&self) -> usize {
        self.workers.len()
    }

    pub fn available_count(&self) -> usize {
        self.workers
            .values()
            .filter(|w| {
                if !w.is_alive {
                    return false;
                }
                match &w.assigned_task {
                    None => true,
                    Some(task_id) => self.is_task_terminal(task_id),
                }
            })
            .count()
    }

    pub fn min_count(&self) -> u32 {
        self.config.min_count
    }

    pub fn max_count(&self) -> u32 {
        self.config.max_count
    }

    pub fn can_spawn_more(&self) -> bool {
        (self.alive_count() as u32) < self.config.max_count
    }

    pub fn all_workers(&self) -> impl Iterator<Item = &WorkerInfo> {
        self.workers.values()
    }

    pub fn alive_workers(&self) -> impl Iterator<Item = &WorkerInfo> {
        self.workers.values().filter(|w| w.is_alive)
    }

    pub fn assignment_history(&self) -> &[WorkerAssignment] {
        &self.assignment_history
    }

    /// Mark workers dead by their session IDs without planning respawns.
    ///
    /// This is a low-level state update helper used by the orchestrator's
    /// production shutdown path so it can record incremental kill successes
    /// without duplicating the pool's internal bookkeeping.
    pub(crate) fn mark_workers_dead_by_sessions(&mut self, sessions: &[SessionId]) {
        for session_id in sessions {
            if let Some(worker_id) = self.session_to_worker.get(session_id).cloned() {
                if let Some(worker) = self.workers.get_mut(&worker_id) {
                    worker.is_alive = false;
                    worker.assigned_task = None;
                }
            }
        }
        self.session_to_worker.retain(|_, worker_id| {
            self.workers
                .get(worker_id)
                .map(|w| w.is_alive)
                .unwrap_or(false)
        });
    }

    /// Best-effort shutdown of all alive worker sessions.
    ///
    /// This is an **internal-only** helper. It marks every worker dead
    /// regardless of whether `kill_session` succeeds and always returns
    /// `Ok(())` so that partial gateway failures do not prevent the pool
    /// from reaching a clean stopped state.  This method is only available
    /// in test builds; `Orchestrator::shutdown()` is the production entry
    /// point that propagates real kill failures so callers know whether
    /// live sessions remain.
    #[cfg(test)]
    pub(crate) async fn shutdown(&mut self) -> Result<()> {
        let alive_workers: Vec<WorkerInfo> = self
            .workers
            .values()
            .filter(|w| w.is_alive)
            .cloned()
            .collect();

        for worker in &alive_workers {
            if let Err(e) = self.gateway.kill_session(&worker.session_id).await {
                warn!(
                    worker_id = %worker.id,
                    session_id = %worker.session_id,
                    error = %e,
                    "Failed to kill worker session during shutdown"
                );
            } else {
                info!(
                    worker_id = %worker.id,
                    session_id = %worker.session_id,
                    "Killed worker session during shutdown"
                );
            }
            // Mark as dead regardless of gateway result so the pool state
            // is consistent.
            if let Some(w) = self.workers.get_mut(&worker.id) {
                w.is_alive = false;
                w.assigned_task = None;
            }
        }

        self.session_to_worker.retain(|_, worker_id| {
            self.workers
                .get(worker_id)
                .map(|w| w.is_alive)
                .unwrap_or(false)
        });

        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_worker_alive_for_test(
        &mut self,
        worker_id: &WorkerId,
        is_alive: bool,
    ) -> Result<()> {
        let worker = self
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| OrchestratorError::WorkerNotFound(worker_id.to_string()))?;
        worker.is_alive = is_alive;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct WorkerPoolOverride {
    pub agent_type: String,
    pub kind: WorkerKind,
    pub count: u32,
}

impl std::fmt::Display for WorkerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerKind::Worker => write!(f, "worker"),
            WorkerKind::Reviewer => write!(f, "reviewer"),
        }
    }
}

impl std::str::FromStr for WorkerKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "worker" => Ok(WorkerKind::Worker),
            "reviewer" => Ok(WorkerKind::Reviewer),
            _ => Err(format!("Unknown worker kind: {}", s)),
        }
    }
}

pub fn parse_worker_overrides(overrides: &str) -> Result<Vec<WorkerPoolOverride>> {
    if overrides.is_empty() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();

    for part in overrides.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parts: Vec<&str> = trimmed.split(':').collect();
        if parts.len() != 2 {
            return Err(OrchestratorError::WorkerPoolError(format!(
                "Invalid worker override format: {}",
                trimmed
            )));
        }

        let agent_type = parts[0].trim().to_string();
        let count: u32 = parts[1].trim().parse().map_err(|_| {
            OrchestratorError::WorkerPoolError(format!("Invalid count in override: {}", parts[1]))
        })?;

        result.push(WorkerPoolOverride {
            agent_type,
            kind: WorkerKind::Worker,
            count,
        });
    }

    Ok(result)
}
