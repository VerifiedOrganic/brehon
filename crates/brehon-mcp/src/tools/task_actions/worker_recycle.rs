use std::io;
use std::path::PathBuf;

use brehon_mux::{Error as MuxError, SessionScopedQueue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::agent::resolve_session_name_for_write;

const LEGACY_SESSION_NAME: &str = "_legacy";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WorkerRecycleEntry {
    pub(crate) task_id: String,
    pub(crate) worker: String,
    pub(crate) requested_at: String,
}

fn worker_recycle_queue_dir(root: &std::path::Path) -> PathBuf {
    root.join("runtime").join("worker-recycle-queue")
}

fn resolve_worker_recycle_session_name(root: &std::path::Path) -> String {
    resolve_session_name_for_write(root).unwrap_or_else(|| LEGACY_SESSION_NAME.to_string())
}

fn worker_recycle_queue(root: &std::path::Path) -> SessionScopedQueue<WorkerRecycleEntry> {
    let session_name = resolve_worker_recycle_session_name(root);
    SessionScopedQueue::new(&session_name, worker_recycle_queue_dir(root))
}

fn into_io_error(err: MuxError) -> io::Error {
    match err {
        MuxError::Io(io) => io,
        other => io::Error::other(other),
    }
}

pub(crate) fn terminal_worker_recycle_candidate(
    task: &serde_json::Map<String, Value>,
) -> Option<String> {
    task.get("assignee")
        .and_then(|value| value.as_str())
        .or_else(|| task.get("review_owner").and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn enqueue_worker_session_recycle(task_id: &str, worker: &str) -> io::Result<()> {
    let worker = worker.trim();
    if worker.is_empty() {
        return Ok(());
    }

    let root = std::env::var_os("BREHON_ROOT")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No BREHON_ROOT"))?;
    let queue = worker_recycle_queue(&root);
    let payload = WorkerRecycleEntry {
        task_id: task_id.to_string(),
        worker: worker.to_string(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    queue.enqueue(payload).map(|_| ()).map_err(into_io_error)
}

/// Outcome of a terminal-close worker-recycle enqueue attempt, including a
/// structured warning payload callers can splice into their response JSON
/// when the enqueue fails. We intentionally do NOT convert the failure into
/// an error response: the task closure itself is already committed on disk
/// and turning a warning into an error would leave the operator unable to
/// observe the successfully-closed task.
#[derive(Debug, Clone)]
pub(crate) struct WorkerRecycleOutcome {
    pub queued: bool,
    pub warning: Option<Value>,
}

/// Attempt to enqueue a recycle request for `worker` after `task_id` reaches
/// a terminal state. On failure, emits a `tracing::error!` (operator-actionable
/// condition — the worker will carry stale context into its next task) and
/// returns a structured warning the caller should include in its response.
///
/// `context` is a short phrase — "integration close", "container close",
/// "terminal close" — that is embedded in both the log line and the warning
/// message so operators can tell which close path produced the failure.
pub(crate) fn enqueue_worker_session_recycle_surfacing(
    task_id: &str,
    recycle_worker: Option<&str>,
    context: &str,
) -> WorkerRecycleOutcome {
    let worker = match recycle_worker.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => w,
        None => {
            return WorkerRecycleOutcome {
                queued: false,
                warning: None,
            };
        }
    };

    match enqueue_worker_session_recycle(task_id, worker) {
        Ok(()) => WorkerRecycleOutcome {
            queued: true,
            warning: None,
        },
        Err(err) => {
            tracing::error!(
                task_id = %task_id,
                worker = %worker,
                context = %context,
                error = %err,
                "Failed to enqueue worker recycle request; worker will carry stale context into its next task unless recycled manually"
            );
            let warning = serde_json::json!({
                "kind": "worker_recycle_enqueue_failed",
                "task_id": task_id,
                "worker": worker,
                "context": context,
                "error": err.to_string(),
                "message": format!(
                    "Task {task_id} closed via {context}, but failed to queue a worker-recycle request for '{worker}'. The worker's pane may retain stale task context until it is recycled manually."
                ),
                "supervisor_action": format!(
                    "Recycle worker '{worker}' manually before assigning new work (brehon-tui Recycle keybind, or kill+respawn the agent pane)."
                )
            });
            WorkerRecycleOutcome {
                queued: false,
                warning: Some(warning),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_mux::{Mux, MuxConfig, PromptQueueEntry};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct ReviewerResetEntryFixture {
        task_id: String,
        review_id: String,
        reviewer: String,
        requested_at: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct DeadLetterEntryFixture {
        original_path: String,
        target: String,
        from: Option<String>,
        message: String,
        error: String,
        reason: String,
        dead_lettered_at: String,
    }

    fn dead_letter_fixture(target: &str, message: &str) -> DeadLetterEntryFixture {
        DeadLetterEntryFixture {
            original_path: format!("/tmp/{target}.prompt"),
            target: target.to_string(),
            from: Some("supervisor".to_string()),
            message: message.to_string(),
            error: "transport failed".to_string(),
            reason: "nonrecoverable prompt delivery failure".to_string(),
            dead_lettered_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn r5_worker_recycle_foreign_session_entry_is_not_inherited() {
        let temp = tempfile::tempdir().expect("create temp queue root");
        let queue_root = temp.path().join("worker-recycle-queue");

        let previous_session_queue = SessionScopedQueue::new("session-prev", queue_root.clone());
        previous_session_queue
            .enqueue(WorkerRecycleEntry {
                task_id: "T-old".to_string(),
                worker: "worker-1".to_string(),
                requested_at: chrono::Utc::now().to_rfc3339(),
            })
            .expect("enqueue previous-session recycle request");

        let current_session_queue =
            SessionScopedQueue::<WorkerRecycleEntry>::new("session-current", queue_root.clone());

        let first_drain: Vec<_> = current_session_queue.drain().collect();
        assert!(
            first_drain.is_empty(),
            "current session should not receive prior-session worker recycle entries"
        );

        let second_drain: Vec<_> = current_session_queue.drain().collect();
        assert!(
            second_drain.is_empty(),
            "swept orphan should not reappear on retry drains"
        );

        assert!(
            !queue_root.join("dead-letter").exists(),
            "foreign session worker recycle entries should not be dead-lettered"
        );
        let previous_session_drain: Vec<_> = previous_session_queue.drain().collect();
        assert_eq!(
            previous_session_drain.len(),
            1,
            "prior-session worker recycle should remain available to its owning session"
        );
    }

    #[test]
    fn r6_new_session_does_not_inherit_in_flight_state_across_all_scoped_queues() {
        let temp = tempfile::tempdir().expect("create temp queue root");
        let shared_queue_root = temp.path();

        let mut mux_config = MuxConfig {
            cwd: shared_queue_root.to_path_buf(),
            session_name: Some("session-B".to_string()),
            workers: 0,
            include_director: false,
            ..MuxConfig::default()
        };
        mux_config.worker_names.clear();
        let mux = Mux::factory(mux_config).expect("create mux for session-B");
        assert_eq!(mux.session_name(), Some("session-B"));
        assert_eq!(
            mux.pending_delayed_prompt_count(),
            0,
            "session-B should begin with no delayed prompt retries"
        );

        let prompt_queue_dir = shared_queue_root.join("prompt-queue");
        let worker_recycle_queue_dir = shared_queue_root.join("worker-recycle-queue");
        let reviewer_reset_queue_dir = shared_queue_root.join("reviewer-reset-queue");
        let dead_letter_queue_dir = shared_queue_root.join("prompt-dead-letter");

        let prompt_a =
            SessionScopedQueue::<PromptQueueEntry>::new("session-A", prompt_queue_dir.clone());
        let worker_a = SessionScopedQueue::<WorkerRecycleEntry>::new(
            "session-A",
            worker_recycle_queue_dir.clone(),
        );
        let reviewer_a = SessionScopedQueue::<ReviewerResetEntryFixture>::new(
            "session-A",
            reviewer_reset_queue_dir.clone(),
        );
        let dead_a = SessionScopedQueue::<DeadLetterEntryFixture>::new(
            "session-A",
            dead_letter_queue_dir.clone(),
        );

        prompt_a
            .enqueue(PromptQueueEntry::new(
                "worker-old",
                Some("supervisor"),
                "stale prompt",
            ))
            .expect("enqueue stale prompt");
        worker_a
            .enqueue(WorkerRecycleEntry {
                task_id: "T-old".to_string(),
                worker: "worker-old".to_string(),
                requested_at: chrono::Utc::now().to_rfc3339(),
            })
            .expect("enqueue stale worker recycle");
        reviewer_a
            .enqueue(ReviewerResetEntryFixture {
                task_id: "T-old".to_string(),
                review_id: "REV-old".to_string(),
                reviewer: "reviewer-old".to_string(),
                requested_at: chrono::Utc::now().to_rfc3339(),
            })
            .expect("enqueue stale reviewer reset");
        dead_a
            .enqueue(dead_letter_fixture("worker-old", "stale dead-letter"))
            .expect("enqueue stale dead letter");

        let prompt_b =
            SessionScopedQueue::<PromptQueueEntry>::new("session-B", prompt_queue_dir.clone());
        let worker_b = SessionScopedQueue::<WorkerRecycleEntry>::new(
            "session-B",
            worker_recycle_queue_dir.clone(),
        );
        let reviewer_b = SessionScopedQueue::<ReviewerResetEntryFixture>::new(
            "session-B",
            reviewer_reset_queue_dir.clone(),
        );
        let dead_b = SessionScopedQueue::<DeadLetterEntryFixture>::new(
            "session-B",
            dead_letter_queue_dir.clone(),
        );

        let prompt_first_drain: Vec<_> = prompt_b.drain().collect();
        let worker_first_drain: Vec<_> = worker_b.drain().collect();
        let reviewer_first_drain: Vec<_> = reviewer_b.drain().collect();
        let dead_first_drain: Vec<_> = dead_b.drain().collect();

        assert!(
            prompt_first_drain.is_empty(),
            "session-B must not deliver prompt entries from session-A"
        );
        assert!(
            worker_first_drain.is_empty(),
            "session-B must not recycle workers from session-A entries"
        );
        assert!(
            reviewer_first_drain.is_empty(),
            "session-B must not reset reviewers from session-A entries"
        );
        assert!(
            dead_first_drain.is_empty(),
            "session-B must not replay dead-letter entries from session-A"
        );

        assert!(
            !prompt_queue_dir.join("dead-letter").exists(),
            "prompt queue should leave foreign-session entries untouched"
        );
        assert!(
            !worker_recycle_queue_dir.join("dead-letter").exists(),
            "worker recycle queue should leave foreign-session entries untouched"
        );
        assert!(
            !reviewer_reset_queue_dir.join("dead-letter").exists(),
            "reviewer reset queue should leave foreign-session entries untouched"
        );
        assert!(
            !dead_letter_queue_dir.join("dead-letter").exists(),
            "dead-letter queue should leave foreign-session entries untouched"
        );

        let prompt_second_drain: Vec<_> = prompt_b.drain().collect();
        let worker_second_drain: Vec<_> = worker_b.drain().collect();
        let reviewer_second_drain: Vec<_> = reviewer_b.drain().collect();
        let dead_second_drain: Vec<_> = dead_b.drain().collect();
        assert!(
            prompt_second_drain.is_empty()
                && worker_second_drain.is_empty()
                && reviewer_second_drain.is_empty()
                && dead_second_drain.is_empty(),
            "foreign-session entries must not be retried or reprocessed in session-B"
        );
    }
}
