use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use brehon_mux::SessionScopedQueue;
use brehon_types::sanitize_runtime_key;
use serde::{Deserialize, Serialize};

use super::helpers::brehon_root;
use super::panel::find_agents_by_role;
use super::state::read_round_request;
use super::tasks::read_task_assignee;
use crate::tools::agent::resolve_session_name_for_write;
use crate::tools::agent::try_deliver_message;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReviewerResetRequest {
    pub(crate) task_id: String,
    pub(crate) review_id: String,
    pub(crate) reviewer: String,
    pub(crate) requested_at: String,
}

pub(crate) type ReviewerResetEntry = ReviewerResetRequest;

const DEFAULT_REVIEWER_RESET_SESSION: &str = "_legacy";

pub(crate) fn reviewer_reset_queue_dir_from_root(root: &Path) -> PathBuf {
    root.join("runtime").join("reviewer-reset-queue")
}

pub(crate) fn reviewer_reset_ack_dir() -> Option<PathBuf> {
    brehon_root().map(|root| root.join("runtime").join("reviewer-reset-acks"))
}

pub(crate) fn reviewer_reset_ack_filename(
    task_id: &str,
    review_id: &str,
    reviewer: &str,
) -> String {
    format!(
        "{}--{}--{}.json",
        sanitize_runtime_key(task_id),
        sanitize_runtime_key(review_id),
        sanitize_runtime_key(reviewer)
    )
}

pub(crate) fn reviewer_reset_ack_exists(task_id: &str, review_id: &str, reviewer: &str) -> bool {
    reviewer_reset_ack_dir()
        .map(|dir| dir.join(reviewer_reset_ack_filename(task_id, review_id, reviewer)))
        .is_some_and(|path| path.exists())
}

fn normalize_requester_target(requested_by: &str, task_assignee: Option<&str>) -> Option<String> {
    let requested_by = requested_by.trim();
    if requested_by.is_empty() {
        return None;
    }

    match requested_by.to_ascii_lowercase().as_str() {
        "worker" | "assignee" | "assigned-worker" | "assigned_worker" | "task-assignee"
        | "task_assignee" => task_assignee
            .map(str::trim)
            .filter(|assignee| !assignee.is_empty())
            .map(String::from),
        "supervisor" | "reviewer" | "reviewers" | "review-panel" | "review_panel" => None,
        _ => Some(requested_by.to_string()),
    }
}

pub(crate) fn enqueue_reviewer_session_reset(
    task_id: &str,
    review_id: &str,
    reviewer: &str,
) -> io::Result<()> {
    let root =
        brehon_root().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No BREHON_ROOT"))?;
    let queue_dir = reviewer_reset_queue_dir_from_root(&root);
    let session_name = resolve_session_name_for_write(&root)
        .unwrap_or_else(|| DEFAULT_REVIEWER_RESET_SESSION.to_string());
    let queue = SessionScopedQueue::<ReviewerResetEntry>::new(&session_name, queue_dir);

    let payload = ReviewerResetEntry {
        task_id: task_id.to_string(),
        review_id: review_id.to_string(),
        reviewer: reviewer.to_string(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    queue
        .enqueue(payload)
        .map(|_| ())
        .map_err(|err| io::Error::other(err.to_string()))
}

/// Deliver a notification to the review requester and all live supervisors.
pub(crate) fn notify_review_stakeholders(
    task_id: &str,
    round: u32,
    from: &str,
    message: &str,
) -> Vec<String> {
    let mut targets = BTreeSet::new();

    if let Some(request) = read_round_request(task_id, round) {
        let assignee = read_task_assignee(task_id);
        if let Some(target) = normalize_requester_target(&request.requested_by, assignee.as_deref())
        {
            targets.insert(target);
        }
    }

    for supervisor in find_agents_by_role("supervisor") {
        if !supervisor.trim().is_empty() {
            targets.insert(supervisor);
        }
    }

    let mut notified = Vec::new();
    for target in targets {
        if try_deliver_message(&target, from, message).queued {
            notified.push(target);
        }
    }
    notified
}

/// Deliver a message to a specific agent via the prompt-queue gateway.
pub(crate) fn notify_agent(target: &str, from: &str, message: &str) {
    let _ = try_deliver_message(target, from, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_reset_queue_leaves_prior_session_entries_for_owner() {
        let temp = tempfile::TempDir::new().expect("create tempdir");
        let queue_dir = temp.path().join("reviewer-reset-queue");

        let prior_session_queue =
            SessionScopedQueue::<ReviewerResetEntry>::new("session-prior", queue_dir.clone());
        prior_session_queue
            .enqueue(ReviewerResetEntry {
                task_id: "T-old".to_string(),
                review_id: "REV-old".to_string(),
                reviewer: "reviewer-alpha".to_string(),
                requested_at: chrono::Utc::now().to_rfc3339(),
            })
            .expect("enqueue prior-session reviewer reset");

        let current_session_queue =
            SessionScopedQueue::<ReviewerResetEntry>::new("session-current", queue_dir.clone());
        let drained: Vec<_> = current_session_queue.drain().collect();
        assert!(
            drained.is_empty(),
            "current session should not drain prior-session entries"
        );

        let dead_letter_dir = queue_dir.join("dead-letter");
        assert!(
            !dead_letter_dir.exists(),
            "foreign-session reviewer reset entries should not be dead-lettered"
        );

        let prior_drained: Vec<_> = prior_session_queue.drain().collect();
        assert_eq!(
            prior_drained.len(),
            1,
            "prior-session reviewer reset entry should remain available to its owning session"
        );
        let active_entry_count = std::fs::read_dir(&queue_dir)
            .expect("queue dir exists")
            .flatten()
            .filter(|entry| entry.file_name() != "dead-letter")
            .count();
        assert_eq!(
            active_entry_count, 0,
            "expected no active reviewer-reset entries after sweep"
        );
    }

    #[test]
    fn requester_role_aliases_do_not_become_literal_prompt_targets() {
        assert_eq!(
            normalize_requester_target("worker", Some("safe-ewe-30")),
            Some("safe-ewe-30".to_string())
        );
        assert_eq!(
            normalize_requester_target(" task_assignee ", Some("firm-hen-20")),
            Some("firm-hen-20".to_string())
        );
        assert_eq!(normalize_requester_target("worker", None), None);
        assert_eq!(
            normalize_requester_target("supervisor", Some("safe-ewe-30")),
            None
        );
        assert_eq!(
            normalize_requester_target("claude-supervisor", Some("safe-ewe-30")),
            Some("claude-supervisor".to_string())
        );
    }
}
