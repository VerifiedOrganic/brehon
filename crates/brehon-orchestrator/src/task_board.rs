//! Task board - in-memory materialized view of task state.
//!
//! The task board is rebuilt from the event stream and provides query methods
//! for tasks by status, assignee, and priority.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;

use brehon_types::{Event, EventId, EventKind, Priority, TaskId, TaskStatus};

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    pub priority: Priority,
    pub assignee: Option<String>,
    pub session_id: Option<String>,
    pub dependencies: Vec<TaskId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub blocked_by: Vec<TaskId>,
    pub last_event_id: Option<EventId>,
}

impl TaskEntry {
    pub fn new(id: TaskId, title: String, description: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            title,
            description,
            status: TaskStatus::Pending,
            priority: Priority::Medium,
            assignee: None,
            session_id: None,
            dependencies: Vec::new(),
            created_at: now,
            updated_at: now,
            blocked_by: Vec::new(),
            last_event_id: None,
        }
    }

    pub fn is_assignable(&self) -> bool {
        matches!(self.status, TaskStatus::Pending) && self.blocked_by.is_empty()
    }

    pub fn is_in_progress(&self) -> bool {
        matches!(self.status, TaskStatus::InProgress)
    }

    pub fn is_complete(&self) -> bool {
        matches!(self.status, TaskStatus::Merged)
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskBoardStats {
    pub total: usize,
    pub pending: usize,
    pub assigned: usize,
    pub in_progress: usize,
    pub in_review: usize,
    pub blocked: usize,
    pub completed: usize,
}

pub struct TaskBoard {
    tasks: RwLock<HashMap<TaskId, TaskEntry>>,
    task_order: RwLock<Vec<TaskId>>,
    review_to_task: RwLock<HashMap<String, TaskId>>,
    max_tasks: usize,
}

impl TaskBoard {
    pub fn new() -> Self {
        Self::with_max_tasks(10_000)
    }

    pub fn with_max_tasks(max_tasks: usize) -> Self {
        debug_assert!(max_tasks > 0, "max_tasks must be > 0");
        Self {
            tasks: RwLock::new(HashMap::new()),
            task_order: RwLock::new(Vec::new()),
            review_to_task: RwLock::new(HashMap::new()),
            max_tasks: max_tasks.max(1),
        }
    }

    pub fn rebuild_from_events(&self, events: Vec<(Event, EventId)>) {
        let mut tasks = HashMap::new();
        let mut order = Vec::new();
        let mut review_to_task = HashMap::new();

        for (event, event_id) in events {
            Self::apply_event_inner(
                &mut tasks,
                &mut order,
                &mut review_to_task,
                &event,
                event_id,
            );
        }

        *self.tasks.write() = tasks;
        *self.task_order.write() = order;
        *self.review_to_task.write() = review_to_task;
    }

    pub fn apply_event(&self, event: &Event, event_id: EventId) {
        let mut tasks = self.tasks.write();
        let mut order = self.task_order.write();
        let mut review_to_task = self.review_to_task.write();
        Self::apply_event_inner(&mut tasks, &mut order, &mut review_to_task, event, event_id);
    }

    fn apply_event_inner(
        tasks: &mut HashMap<TaskId, TaskEntry>,
        order: &mut Vec<TaskId>,
        review_to_task: &mut HashMap<String, TaskId>,
        event: &Event,
        event_id: EventId,
    ) {
        match &event.kind {
            EventKind::TaskCreated { task_id } => {
                let id = TaskId::new(task_id);
                if !tasks.contains_key(&id) {
                    let entry = TaskEntry::new(id.clone(), String::new(), String::new());
                    tasks.insert(id.clone(), entry);
                    order.push(id);
                }
            }
            EventKind::TaskAssigned { task_id, agent_id } => {
                let id = TaskId::new(task_id);
                if let Some(task) = tasks.get_mut(&id) {
                    task.status = TaskStatus::Assigned;
                    task.assignee = Some(agent_id.clone());
                    task.updated_at = event.timestamp;
                    task.last_event_id = Some(event_id);
                }
            }
            EventKind::TaskCompleted { task_id } => {
                let id = TaskId::new(task_id);
                if let Some(task) = tasks.get_mut(&id) {
                    task.status = TaskStatus::InReview;
                    task.updated_at = event.timestamp;
                    task.last_event_id = Some(event_id);
                }
            }
            EventKind::ReviewRequested { task_id, .. } => {
                let id = TaskId::new(task_id);
                if let Some(task) = tasks.get_mut(&id) {
                    task.status = TaskStatus::InReview;
                    task.updated_at = event.timestamp;
                    task.last_event_id = Some(event_id);
                }
                if let EventKind::ReviewRequested { review_id, .. } = &event.kind {
                    review_to_task.insert(review_id.clone(), id);
                }
            }
            EventKind::ReviewApproved { review_id } => {
                if let Some(task_id) = review_to_task.get(review_id) {
                    if let Some(task) = tasks.get_mut(task_id) {
                        task.status = TaskStatus::Approved;
                        task.updated_at = event.timestamp;
                        task.last_event_id = Some(event_id);
                    }
                }
            }
            EventKind::ReviewRejected { review_id } => {
                if let Some(task_id) = review_to_task.get(review_id) {
                    if let Some(task) = tasks.get_mut(task_id) {
                        task.status = TaskStatus::ChangesRequested;
                        task.updated_at = event.timestamp;
                        task.last_event_id = Some(event_id);
                    }
                }
            }
            EventKind::ReviewChangesRequested { review_id } => {
                if let Some(task_id) = review_to_task.get(review_id) {
                    if let Some(task) = tasks.get_mut(task_id) {
                        task.status = TaskStatus::ChangesRequested;
                        task.updated_at = event.timestamp;
                        task.last_event_id = Some(event_id);
                    }
                }
            }
            EventKind::MergeCommitted { task_id } => {
                let id = TaskId::new(task_id);
                if let Some(task) = tasks.get_mut(&id) {
                    task.status = TaskStatus::Merged;
                    task.updated_at = event.timestamp;
                    task.last_event_id = Some(event_id);
                }
            }
            EventKind::MergeAborted { task_id, .. } => {
                let id = TaskId::new(task_id);
                if let Some(task) = tasks.get_mut(&id) {
                    task.status = TaskStatus::InProgress;
                    task.updated_at = event.timestamp;
                    task.last_event_id = Some(event_id);
                }
            }
            EventKind::AgentDied { session_id, .. } => {
                for task in tasks.values_mut() {
                    if task.session_id.as_deref() == Some(session_id.as_str())
                        && task.status == TaskStatus::InProgress
                    {
                        task.status = TaskStatus::Pending;
                        task.session_id = None;
                        task.updated_at = event.timestamp;
                        task.last_event_id = Some(event_id);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn has_task(&self, task_id: &TaskId) -> bool {
        self.tasks.read().contains_key(task_id)
    }

    pub fn get_task(&self, task_id: &TaskId) -> Option<TaskEntry> {
        self.tasks.read().get(task_id).cloned()
    }

    pub fn get_tasks_by_status(&self, status: TaskStatus) -> Vec<TaskEntry> {
        self.tasks
            .read()
            .values()
            .filter(|t| t.status == status)
            .cloned()
            .collect()
    }

    pub fn get_tasks_by_assignee(&self, assignee: &str) -> Vec<TaskEntry> {
        self.tasks
            .read()
            .values()
            .filter(|t| t.assignee.as_deref() == Some(assignee))
            .cloned()
            .collect()
    }

    pub fn get_tasks_by_priority(&self, priority: Priority) -> Vec<TaskEntry> {
        self.tasks
            .read()
            .values()
            .filter(|t| t.priority == priority)
            .cloned()
            .collect()
    }

    pub fn get_pending_tasks(&self) -> Vec<TaskEntry> {
        self.get_tasks_by_status(TaskStatus::Pending)
    }

    pub fn get_assignable_tasks(&self) -> Vec<TaskEntry> {
        self.tasks
            .read()
            .values()
            .filter(|t| t.is_assignable())
            .cloned()
            .collect()
    }

    pub fn get_in_progress_tasks(&self) -> Vec<TaskEntry> {
        self.get_tasks_by_status(TaskStatus::InProgress)
    }

    pub fn get_blocked_tasks(&self) -> Vec<TaskEntry> {
        self.get_tasks_by_status(TaskStatus::Blocked)
    }

    pub fn get_complete_tasks(&self) -> Vec<TaskEntry> {
        self.tasks
            .read()
            .values()
            .filter(|t| t.is_complete())
            .cloned()
            .collect()
    }

    pub fn update_task_status(&self, task_id: &TaskId, status: TaskStatus) -> Result<()> {
        let mut tasks = self.tasks.write();
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = status;
            task.updated_at = Utc::now();
            Ok(())
        } else {
            Err(crate::error::OrchestratorError::TaskNotFound(
                task_id.to_string(),
            ))
        }
    }

    pub fn assign_task(
        &self,
        task_id: &TaskId,
        assignee: &str,
        session_id: Option<&str>,
    ) -> Result<()> {
        let mut tasks = self.tasks.write();
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = TaskStatus::Assigned;
            task.assignee = Some(assignee.to_string());
            task.session_id = session_id.map(|s| s.to_string());
            task.updated_at = Utc::now();
            Ok(())
        } else {
            Err(crate::error::OrchestratorError::TaskNotFound(
                task_id.to_string(),
            ))
        }
    }

    pub fn unassign_task(&self, task_id: &TaskId) -> Result<()> {
        let mut tasks = self.tasks.write();
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = TaskStatus::Pending;
            task.assignee = None;
            task.session_id = None;
            task.updated_at = Utc::now();
            Ok(())
        } else {
            Err(crate::error::OrchestratorError::TaskNotFound(
                task_id.to_string(),
            ))
        }
    }

    pub fn set_blocked(&self, task_id: &TaskId, blocked_by: Vec<TaskId>) -> Result<()> {
        let mut tasks = self.tasks.write();
        if let Some(task) = tasks.get_mut(task_id) {
            if task.status == TaskStatus::Pending || task.status == TaskStatus::Blocked {
                task.status = TaskStatus::Blocked;
            }
            task.blocked_by = blocked_by;
            task.updated_at = Utc::now();
            Ok(())
        } else {
            Err(crate::error::OrchestratorError::TaskNotFound(
                task_id.to_string(),
            ))
        }
    }

    pub fn clear_blocked(&self, task_id: &TaskId) -> Result<()> {
        let mut tasks = self.tasks.write();
        if let Some(task) = tasks.get_mut(task_id) {
            if task.status == TaskStatus::Blocked && task.blocked_by.is_empty() {
                task.status = TaskStatus::Pending;
            }
            task.updated_at = Utc::now();
            Ok(())
        } else {
            Err(crate::error::OrchestratorError::TaskNotFound(
                task_id.to_string(),
            ))
        }
    }

    pub fn add_task(&self, task: TaskEntry) {
        let id = task.id.clone();
        let mut tasks = self.tasks.write();
        let mut order = self.task_order.write();

        if !tasks.contains_key(&id) {
            order.push(id.clone());
        }
        tasks.insert(id, task);
    }

    pub fn remove_task(&self, task_id: &TaskId) -> Option<TaskEntry> {
        let mut tasks = self.tasks.write();
        let mut order = self.task_order.write();
        let mut review_to_task = self.review_to_task.write();
        order.retain(|id| id != task_id);
        review_to_task.retain(|_, mapped_task_id| mapped_task_id != task_id);
        tasks.remove(task_id)
    }

    pub fn all_tasks(&self) -> Vec<TaskEntry> {
        let order = self.task_order.read();
        let tasks = self.tasks.read();
        order
            .iter()
            .filter_map(|id| tasks.get(id).cloned())
            .collect()
    }

    pub fn stats(&self) -> TaskBoardStats {
        let tasks = self.tasks.read();
        let total = tasks.len();
        let pending = tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .count();
        let assigned = tasks
            .values()
            .filter(|t| t.status == TaskStatus::Assigned)
            .count();
        let in_progress = tasks
            .values()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count();
        let in_review = tasks
            .values()
            .filter(|t| t.status == TaskStatus::InReview)
            .count();
        let blocked = tasks
            .values()
            .filter(|t| t.status == TaskStatus::Blocked)
            .count();
        let completed = tasks
            .values()
            .filter(|t| t.status == TaskStatus::Merged)
            .count();

        TaskBoardStats {
            total,
            pending,
            assigned,
            in_progress,
            in_review,
            blocked,
            completed,
        }
    }

    pub fn len(&self) -> usize {
        self.tasks.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.read().is_empty()
    }

    pub fn clear(&self) {
        self.tasks.write().clear();
        self.task_order.write().clear();
        self.review_to_task.write().clear();
    }

    pub fn set_max_tasks(&mut self, max: usize) {
        self.max_tasks = max;
    }

    /// Remove oldest merged tasks to keep the total count within bounds.
    /// Returns the number of tasks removed.
    ///
    /// This is best-effort: only terminal (Merged) tasks are evicted. If
    /// there are not enough merged tasks, the board may remain above the
    /// limit until more tasks reach a terminal state.
    pub fn apply_bounds(&self) -> usize {
        let mut tasks = self.tasks.write();
        let mut order = self.task_order.write();
        let mut review_to_task = self.review_to_task.write();

        if tasks.len() <= self.max_tasks {
            return 0;
        }

        let to_remove = tasks.len() - self.max_tasks;
        let mut removed_ids = Vec::with_capacity(to_remove);
        let mut removed = 0usize;
        let mut i = 0;

        // First pass: identify merged tasks to evict (oldest first).
        while removed < to_remove && i < order.len() {
            let task_id = &order[i];
            if let Some(task) = tasks.get(task_id) {
                if task.status == TaskStatus::Merged {
                    removed_ids.push(task_id.clone());
                    removed += 1;
                }
            }
            i += 1;
        }

        // Batch remove from all structures.
        for task_id in &removed_ids {
            tasks.remove(task_id);
        }

        // Build a HashSet for O(1) lookup during retain.
        let removed_set: std::collections::HashSet<_> = removed_ids.iter().collect();
        review_to_task.retain(|_, mapped| !removed_set.contains(mapped));

        // Remove evicted IDs from order in a single pass.
        order.retain(|id| !removed_set.contains(id));

        removed
    }
}

impl Default for TaskBoard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_event(kind: EventKind, aggregate_id: &str) -> (Event, EventId) {
        (
            Event {
                kind,
                timestamp: Utc::now(),
                aggregate_id: aggregate_id.to_string(),
            },
            EventId::new(1),
        )
    }

    #[test]
    fn task_board_new() {
        let board = TaskBoard::new();
        assert!(board.is_empty());
        assert_eq!(board.len(), 0);
    }

    #[test]
    fn remove_task() {
        let board = TaskBoard::new();
        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".to_string(),
            "Description".to_string(),
        );

        board.add_task(task);
        assert_eq!(board.len(), 1);

        let removed = board.remove_task(&TaskId::new("T001"));
        assert!(removed.is_some());
        assert_eq!(board.len(), 0);
    }

    #[test]
    fn review_approval_moves_task_to_approved() {
        let board = TaskBoard::new();

        board.apply_event(
            &Event {
                kind: EventKind::TaskCreated {
                    task_id: "T001".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".to_string(),
            },
            EventId::new(1),
        );
        board.apply_event(
            &Event {
                kind: EventKind::ReviewRequested {
                    task_id: "T001".to_string(),
                    review_id: "REV-1".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: "REV-1".to_string(),
            },
            EventId::new(2),
        );
        board.apply_event(
            &Event {
                kind: EventKind::ReviewApproved {
                    review_id: "REV-1".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: "REV-1".to_string(),
            },
            EventId::new(3),
        );

        let task = board.get_task(&TaskId::new("T001")).unwrap();
        assert_eq!(task.status, TaskStatus::Approved);
    }

    #[test]
    fn review_changes_requested_moves_task_out_of_in_review() {
        let board = TaskBoard::new();

        board.apply_event(
            &Event {
                kind: EventKind::TaskCreated {
                    task_id: "T001".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".to_string(),
            },
            EventId::new(1),
        );
        board.apply_event(
            &Event {
                kind: EventKind::ReviewRequested {
                    task_id: "T001".to_string(),
                    review_id: "REV-1".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: "REV-1".to_string(),
            },
            EventId::new(2),
        );
        board.apply_event(
            &Event {
                kind: EventKind::ReviewChangesRequested {
                    review_id: "REV-1".to_string(),
                },
                timestamp: Utc::now(),
                aggregate_id: "REV-1".to_string(),
            },
            EventId::new(3),
        );

        let task = board.get_task(&TaskId::new("T001")).unwrap();
        assert_eq!(task.status, TaskStatus::ChangesRequested);
    }

    #[test]
    fn update_task_status() {
        let board = TaskBoard::new();
        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".to_string(),
            "Description".to_string(),
        );

        board.add_task(task);

        board
            .update_task_status(&TaskId::new("T001"), TaskStatus::InProgress)
            .unwrap();

        let updated = board.get_task(&TaskId::new("T001")).unwrap();
        assert_eq!(updated.status, TaskStatus::InProgress);
    }

    #[test]
    fn assign_task() {
        let board = TaskBoard::new();
        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".to_string(),
            "Description".to_string(),
        );

        board.add_task(task);

        board
            .assign_task(&TaskId::new("T001"), "agent-1", Some("session-1"))
            .unwrap();

        let updated = board.get_task(&TaskId::new("T001")).unwrap();
        assert_eq!(updated.status, TaskStatus::Assigned);
        assert_eq!(updated.assignee, Some("agent-1".to_string()));
        assert_eq!(updated.session_id, Some("session-1".to_string()));
    }

    #[test]
    fn unassign_task() {
        let board = TaskBoard::new();
        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".to_string(),
            "Description".to_string(),
        );

        board.add_task(task);
        board
            .assign_task(&TaskId::new("T001"), "agent-1", None)
            .unwrap();

        board.unassign_task(&TaskId::new("T001")).unwrap();

        let updated = board.get_task(&TaskId::new("T001")).unwrap();
        assert_eq!(updated.status, TaskStatus::Pending);
        assert!(updated.assignee.is_none());
    }

    #[test]
    fn set_blocked_clear_blocked() {
        let board = TaskBoard::new();
        let task = TaskEntry::new(
            TaskId::new("T001"),
            "Test task".to_string(),
            "Description".to_string(),
        );

        board.add_task(task);

        board
            .set_blocked(&TaskId::new("T001"), vec![TaskId::new("T002")])
            .unwrap();

        let updated = board.get_task(&TaskId::new("T001")).unwrap();
        assert_eq!(updated.status, TaskStatus::Blocked);
        assert_eq!(updated.blocked_by.len(), 1);

        board.clear_blocked(&TaskId::new("T001")).unwrap();
    }

    #[test]
    fn rebuild_from_events() {
        let board = TaskBoard::new();

        let events = vec![
            create_event(
                EventKind::TaskCreated {
                    task_id: "T001".into(),
                },
                "T001",
            ),
            create_event(
                EventKind::TaskCreated {
                    task_id: "T002".into(),
                },
                "T002",
            ),
        ];

        board.rebuild_from_events(events);

        assert_eq!(board.len(), 2);
        assert!(board.get_task(&TaskId::new("T001")).is_some());
        assert!(board.get_task(&TaskId::new("T002")).is_some());
    }

    #[test]
    fn get_tasks_by_status() {
        let board = TaskBoard::new();

        let mut task1 = TaskEntry::new(TaskId::new("T001"), "Task 1".into(), "Desc".into());
        task1.status = TaskStatus::Pending;

        let mut task2 = TaskEntry::new(TaskId::new("T002"), "Task 2".into(), "Desc".into());
        task2.status = TaskStatus::InProgress;

        let mut task3 = TaskEntry::new(TaskId::new("T003"), "Task 3".into(), "Desc".into());
        task3.status = TaskStatus::InProgress;

        board.add_task(task1);
        board.add_task(task2);
        board.add_task(task3);

        let in_progress = board.get_tasks_by_status(TaskStatus::InProgress);
        assert_eq!(in_progress.len(), 2);

        let pending = board.get_tasks_by_status(TaskStatus::Pending);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn task_entry_is_assignable() {
        let mut task = TaskEntry::new(TaskId::new("T001"), "Test".into(), "Desc".into());

        assert!(task.is_assignable());

        task.status = TaskStatus::Assigned;
        assert!(!task.is_assignable());

        task.status = TaskStatus::Pending;
        task.blocked_by.push(TaskId::new("T002"));
        assert!(!task.is_assignable());
    }

    #[test]
    fn stats() {
        let board = TaskBoard::new();

        let mut task1 = TaskEntry::new(TaskId::new("T001"), "Task 1".into(), "Desc".into());
        task1.status = TaskStatus::Pending;

        let mut task2 = TaskEntry::new(TaskId::new("T002"), "Task 2".into(), "Desc".into());
        task2.status = TaskStatus::InProgress;

        let mut task3 = TaskEntry::new(TaskId::new("T003"), "Task 3".into(), "Desc".into());
        task3.status = TaskStatus::Merged;

        board.add_task(task1);
        board.add_task(task2);
        board.add_task(task3);

        let stats = board.stats();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.in_progress, 1);
        assert_eq!(stats.completed, 1);
    }

    #[test]
    fn agent_death_reassigns_task() {
        let board = TaskBoard::new();

        let mut task1 = TaskEntry::new(TaskId::new("T001"), "Task 1".into(), "Desc".into());
        task1.status = TaskStatus::InProgress;
        task1.session_id = Some("session-1".into());

        board.add_task(task1);

        let event = Event {
            kind: EventKind::AgentDied {
                agent_id: "agent-1".into(),
                session_id: "session-1".into(),
                reason: "crashed".into(),
            },
            timestamp: Utc::now(),
            aggregate_id: "system".into(),
        };

        board.apply_event(&event, EventId::new(2));

        let updated = board.get_task(&TaskId::new("T001")).unwrap();
        assert_eq!(updated.status, TaskStatus::Pending);
        assert!(updated.session_id.is_none());
    }

    #[test]
    fn apply_bounds_evicts_oldest_merged_tasks() {
        let mut board = TaskBoard::new();
        board.tasks.write().insert(
            TaskId::new("T-old"),
            TaskEntry {
                id: TaskId::new("T-old"),
                title: "Old".into(),
                description: "Desc".into(),
                status: TaskStatus::Merged,
                priority: Priority::Medium,
                assignee: None,
                session_id: None,
                dependencies: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
                blocked_by: vec![],
                last_event_id: None,
            },
        );
        board.task_order.write().push(TaskId::new("T-old"));

        for i in 0..5 {
            let mut task =
                TaskEntry::new(TaskId::new(format!("T{}", i)), "Task".into(), "Desc".into());
            task.status = TaskStatus::Pending;
            board.add_task(task);
        }

        board.set_max_tasks(3);
        let removed = board.apply_bounds();
        assert_eq!(removed, 1);
        assert!(board.get_task(&TaskId::new("T-old")).is_none());
        assert_eq!(board.len(), 5);
    }
}
