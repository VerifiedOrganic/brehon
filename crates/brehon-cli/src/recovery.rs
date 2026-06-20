//! Recovery scan for startup state reconciliation.
//!
//! Detects orphaned tasks, unfinished merges, and other incomplete states
//! from crash recovery.

use std::sync::Arc;

use anyhow::Result;
use brehon_ports::EventStore;
use brehon_types::{EventId, EventKind, TaskStatus};

/// Page size for the startup recovery scan.
///
/// The log can grow without bound across a multi-day unattended session, so the
/// scan pages through it instead of loading it all at once, keeping memory bounded
/// to one page plus the accumulated (task/session-keyed) maps.
const RECOVERY_PAGE_SIZE: usize = 10_000;

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

/// Accumulated reconstruction of task/session/merge state from the event log.
struct RecoveryMaps {
    task_states: std::collections::HashMap<String, TaskStatus>,
    task_assignments: std::collections::HashMap<String, String>,
    active_sessions: std::collections::HashMap<String, bool>,
    merge_states: std::collections::HashMap<String, MergeState>,
    scanned: usize,
}

/// Page through the entire event log, folding it into the recovery maps.
///
/// Memory is bounded to a single page plus the maps (which are sized by the
/// number of distinct tasks/sessions, not the event count). The cursor advances
/// to the last EventId of each page (`stream` is exclusive of `since`), and the
/// scan stops only on an empty page: a short page is NOT a reliable end-of-log
/// signal because the store may skip archived/compacted sequence numbers.
async fn collect_recovery_state(
    event_store: &Arc<dyn EventStore>,
    page_size: usize,
) -> Result<RecoveryMaps> {
    let mut task_states: std::collections::HashMap<String, TaskStatus> =
        std::collections::HashMap::new();
    let mut task_assignments: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut active_sessions: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    let mut merge_states: std::collections::HashMap<String, MergeState> =
        std::collections::HashMap::new();

    let mut cursor: Option<EventId> = None;
    let mut scanned: usize = 0;

    loop {
        let page = event_store.stream(cursor, page_size).await?;
        if page.is_empty() {
            break;
        }
        scanned += page.len();
        tracing::debug!("Recovery: scanned page of {} events", page.len());

        for (event, _event_id) in &page {
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

        // `EventId` is `Copy`, so no clone; the empty-page break above guarantees
        // `last()` is `Some`, so the cursor always advances (no infinite loop).
        cursor = page.last().map(|(_, id)| *id);
    }

    Ok(RecoveryMaps {
        task_states,
        task_assignments,
        active_sessions,
        merge_states,
        scanned,
    })
}

pub async fn run_recovery(event_store: &Arc<dyn EventStore>) -> Result<Vec<String>> {
    tracing::info!("Running startup recovery scan...");

    let RecoveryMaps {
        task_states,
        task_assignments,
        active_sessions,
        merge_states,
        scanned,
    } = collect_recovery_state(event_store, RECOVERY_PAGE_SIZE).await?;

    tracing::info!("Recovery: Scanned {} events across pages", scanned);

    let mut report = RecoveryReport {
        orphaned_tasks: Vec::new(),
        unfinished_merges: Vec::new(),
        expired_sessions: Vec::new(),
        messages: Vec::new(),
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    use brehon_test_harness::InMemoryEventStore;
    use brehon_types::Event;
    use chrono::Utc;

    fn event(kind: EventKind, aggregate_id: &str) -> Event {
        Event {
            kind,
            timestamp: Utc::now(),
            aggregate_id: aggregate_id.into(),
        }
    }

    /// An orphaned task whose events fall beyond the first page must still be
    /// detected: with a tiny page size the assignment/death land on later pages,
    /// exercising the multi-page cursor traversal.
    #[tokio::test]
    async fn recovery_detects_orphan_beyond_first_page() {
        let store = InMemoryEventStore::new();

        // The orphaned task is created first.
        store
            .append(event(
                EventKind::TaskCreated {
                    task_id: "T-orphan".into(),
                },
                "T-orphan",
            ))
            .await
            .unwrap();

        // Filler events push the orphan's assignment/death past the first page.
        for i in 0..10 {
            let task_id = format!("T-filler-{i}");
            store
                .append(event(
                    EventKind::TaskCreated {
                        task_id: task_id.clone(),
                    },
                    &task_id,
                ))
                .await
                .unwrap();
        }

        // Assign the orphan to an agent, then kill that agent: the assignment
        // remains but the session is dead, so the task is orphaned.
        store
            .append(event(
                EventKind::AgentSpawned {
                    agent_id: "agent-x".into(),
                    session_id: "agent-x".into(),
                    role: "worker".into(),
                },
                "agent-x",
            ))
            .await
            .unwrap();
        store
            .append(event(
                EventKind::TaskAssigned {
                    task_id: "T-orphan".into(),
                    agent_id: "agent-x".into(),
                },
                "T-orphan",
            ))
            .await
            .unwrap();
        store
            .append(event(
                EventKind::AgentDied {
                    agent_id: "agent-x".into(),
                    session_id: "agent-x".into(),
                    reason: "crash".into(),
                },
                "agent-x",
            ))
            .await
            .unwrap();

        let store: Arc<dyn EventStore> = Arc::new(store);

        // A tiny page size forces the orphan's later events onto pages 2+ while
        // keeping the test to a handful of appends.
        let maps = collect_recovery_state(&store, 4).await.unwrap();
        assert_eq!(maps.scanned, 14, "every event across all pages is scanned");
        assert_eq!(
            maps.task_assignments.get("T-orphan").map(String::as_str),
            Some("agent-x"),
            "assignment recorded from a later page"
        );
        assert_eq!(maps.active_sessions.get("agent-x"), Some(&false));

        // The full recovery (default page size) reports the orphan.
        let messages = run_recovery(&store).await.unwrap();
        assert!(
            messages.iter().any(|m| m.contains("T-orphan")),
            "orphan beyond the first page should be reported, got: {messages:?}"
        );
    }

    /// A short final page must not terminate the scan early: with a page size
    /// that does not evenly divide the log, the tail page is shorter than the
    /// limit yet must still be folded in.
    #[tokio::test]
    async fn recovery_scans_short_final_page() {
        let store = InMemoryEventStore::new();

        for i in 0..7 {
            let task_id = format!("T-{i}");
            store
                .append(event(
                    EventKind::TaskCreated {
                        task_id: task_id.clone(),
                    },
                    &task_id,
                ))
                .await
                .unwrap();
        }

        let store: Arc<dyn EventStore> = Arc::new(store);

        // 7 events with page size 4 -> pages of [4, 3]; the empty-page break
        // still fires after the short page because the cursor advanced.
        let maps = collect_recovery_state(&store, 4).await.unwrap();
        assert_eq!(maps.scanned, 7);
        assert_eq!(maps.task_states.len(), 7);
    }
}
