//! Recovery scan for startup state reconciliation.
//!
//! Detects orphaned tasks, unfinished merges, and other incomplete states
//! from crash recovery.

use std::sync::Arc;

use anyhow::Result;
use brehon_ports::EventStore;
use brehon_types::{EventKind, TaskStatus};

pub struct RecoveryReport {
    pub orphaned_tasks: Vec<String>,
    pub unfinished_merges: Vec<String>,
    pub expired_sessions: Vec<String>,
    pub messages: Vec<String>,
}

impl RecoveryReport {
    pub fn is_empty(&self) -> bool {
        self.orphaned_tasks.is_empty()
            && self.unfinished_merges.is_empty()
            && self.expired_sessions.is_empty()
    }

    pub fn total_findings(&self) -> usize {
        self.orphaned_tasks.len() + self.unfinished_merges.len() + self.expired_sessions.len()
    }
}

pub async fn run_recovery(event_store: &Arc<dyn EventStore>) -> Result<Vec<String>> {
    tracing::info!("Running startup recovery scan...");

    let events = event_store.stream(None, 10000).await?;

    tracing::info!("Recovery: Scanning {} events", events.len());

    let mut report = RecoveryReport {
        orphaned_tasks: Vec::new(),
        unfinished_merges: Vec::new(),
        expired_sessions: Vec::new(),
        messages: Vec::new(),
    };

    let mut task_states: std::collections::HashMap<String, TaskStatus> =
        std::collections::HashMap::new();
    let mut task_assignments: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut active_sessions: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    let mut merge_states: std::collections::HashMap<String, MergeState> =
        std::collections::HashMap::new();

    for (event, _event_id) in &events {
        match &event.kind {
            EventKind::TaskCreated { task_id } => {
                task_states.insert(task_id.clone(), TaskStatus::Pending);
            }
            EventKind::TaskAssigned { task_id, agent_id } => {
                task_states.insert(task_id.clone(), TaskStatus::Assigned);
                task_assignments.insert(task_id.clone(), agent_id.clone());
            }
            EventKind::TaskCompleted { task_id } => {
                task_states.insert(task_id.clone(), TaskStatus::Approved);
            }
            EventKind::AgentSpawned { session_id, .. } => {
                active_sessions.insert(session_id.clone(), true);
            }
            EventKind::AgentDied { session_id, .. } => {
                active_sessions.insert(session_id.clone(), false);
            }
            EventKind::MergePrepared { task_id, .. } => {
                merge_states.insert(task_id.clone(), MergeState::Prepared);
            }
            EventKind::MergeCommitted { task_id, .. } => {
                merge_states.insert(task_id.to_string(), MergeState::Committed);
            }
            EventKind::MergeAborted { task_id, .. } => {
                merge_states.insert(task_id.to_string(), MergeState::Aborted);
            }
            _ => {}
        }
    }

    for (task_id, status) in &task_states {
        match status {
            TaskStatus::InProgress | TaskStatus::Assigned => {
                if let Some(agent_id) = task_assignments.get(task_id) {
                    let agent_active = active_sessions.get(agent_id).copied().unwrap_or(false);
                    if !agent_active {
                        report.orphaned_tasks.push(task_id.clone());
                        report.messages.push(format!(
                            "Task {} is {:?} but assigned agent {} is not active - needs reassignment",
                            task_id, status, agent_id
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    for (task_id, state) in &merge_states {
        if matches!(state, MergeState::Prepared) {
            report.unfinished_merges.push(task_id.clone());
            report.messages.push(format!(
                "Task {} has prepared merge that was never committed or aborted",
                task_id
            ));
        }
    }

    for (session_id, active) in &active_sessions {
        if !active {
            report.expired_sessions.push(session_id.clone());
        }
    }

    if report.is_empty() {
        tracing::info!("Recovery complete: No issues found");
    } else {
        tracing::warn!(
            "Recovery complete: {} findings ({} orphaned tasks, {} unfinished merges, {} expired sessions)",
            report.total_findings(),
            report.orphaned_tasks.len(),
            report.unfinished_merges.len(),
            report.expired_sessions.len()
        );
    }

    Ok(report.messages)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MergeState {
    Prepared,
    Committed,
    Aborted,
}
