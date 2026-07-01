use std::collections::HashSet;

use serde_json::Value;

use crate::tools::task_actions::enqueue_worker_session_recycle_surfacing;

use super::worktree_ops::{
    archive_worktree_with_git2, check_worktree_state_with_git2, find_worktree_by_worker,
    remove_worktree_with_git2,
};

#[derive(Default)]
pub(super) struct ForcedReassignment {
    from: Option<String>,
    note: Option<String>,
    recycle_target: Option<String>,
    recycle_warning: Option<Value>,
}

impl ForcedReassignment {
    pub(super) fn record_live_transfer(
        &mut self,
        task: &mut serde_json::Map<String, Value>,
        old_assignee: &str,
        new_assignee: &str,
        normalized_status: &str,
    ) {
        let note = format!(
            "Supervisor force-reassigned active task from {normalized_status}: previous live assignee {old_assignee} was replaced by {new_assignee}. \
             Previous worker recycle was queued to clear stale assignment context."
        );
        task.insert("reassignment_note".into(), Value::String(note.clone()));
        self.from = Some(old_assignee.to_string());
        self.note = Some(note);
        self.recycle_target = Some(old_assignee.to_string());
    }

    pub(super) fn queue_previous_worker_recycle(&mut self, task_id: &str) {
        if let Some(worker) = self.recycle_target.as_deref() {
            self.recycle_warning = enqueue_worker_session_recycle_surfacing(
                task_id,
                Some(worker),
                "forced live worker reassignment",
            )
            .warning;
        }
    }

    pub(super) fn append_result_fields(&mut self, result: &mut Value) {
        if let Some(previous) = self.from.as_deref() {
            result["reassigned_from"] = Value::String(previous.to_string());
            result["force_reassigned"] = Value::Bool(true);
        }
        if let Some(note) = self.note.as_deref() {
            result["reassignment_note"] = Value::String(note.to_string());
        }
        if let Some(worker) = self.recycle_target.as_deref() {
            result["previous_worker_recycle_queued"] = Value::Bool(self.recycle_warning.is_none());
            result["previous_worker_recycle_target"] = Value::String(worker.to_string());
        }
        if let Some(warning) = self.recycle_warning.take() {
            result["previous_worker_recycle_warning"] = warning;
        }
    }
}

pub(super) fn is_live_active_reassignment(
    normalized_status: &str,
    previous_assignee: Option<&str>,
    assignee: &str,
    live_workers: &HashSet<String>,
) -> bool {
    matches!(
        normalized_status,
        "assigned" | "in_progress" | "changes_requested"
    ) && previous_assignee
        .is_some_and(|existing| existing != assignee && live_workers.contains(existing))
}

pub(super) fn live_reassignment_requires_force_message(
    task_id: &str,
    existing_owner: &str,
    assignee: &str,
) -> String {
    format!(
        "Cannot reassign task {task_id} from live worker '{existing_owner}' to '{assignee}' without force_reassign=true. \
         If the worker is stalled, ignored a delivered prompt, or needs replacement, retry with factory action=assign_workers task_id={task_id} worker={assignee} force_reassign=true. \
         Brehon will transfer ownership and queue a recycle for the previous worker to clear stale context."
    )
}

pub(super) fn reassigned_worktree_state(task: &serde_json::Map<String, Value>) -> String {
    if task
        .get("worktree_archived")
        .and_then(|v| v.as_str())
        .is_some()
    {
        "archived".to_string()
    } else {
        "preserved".to_string()
    }
}

pub(super) fn prepare_previous_worktree_for_reassignment(
    task_id: &str,
    task: &mut serde_json::Map<String, Value>,
    old_assignee: &str,
    force_reassign: bool,
    remove_clean_worktree: bool,
) -> Result<(), String> {
    match find_worktree_by_worker(old_assignee) {
        Ok(Some((repo, worktree_path))) => {
            match check_worktree_state_with_git2(&repo, &worktree_path) {
                Ok(state) => {
                    use brehon_git::WorktreeStateCheck;
                    match state {
                        WorktreeStateCheck::Clean => {
                            if remove_clean_worktree {
                                if let Err(e) = remove_worktree_with_git2(&repo, &worktree_path) {
                                    tracing::warn!(
                                        "Failed to remove clean worktree for {}: {}",
                                        old_assignee,
                                        e
                                    );
                                }
                            }
                        }
                        WorktreeStateCheck::Missing => {}
                        WorktreeStateCheck::Dirty { details }
                        | WorktreeStateCheck::MidOperation { operation: details } => {
                            if !force_reassign {
                                return Err(format!(
                                "Cannot reassign task {task_id}: old worker '{old_assignee}' has dirty worktree ({details}) \
                                 Use force_reassign=true to archive the worktree and proceed."
                            ));
                            }
                            let archive_path = archive_worktree_with_git2(
                                &repo,
                                &worktree_path,
                                old_assignee,
                                task_id,
                                "reassignment",
                            )
                            .map_err(|e| {
                                format!("Failed to archive worktree for {old_assignee}: {e}")
                            })?;
                            tracing::info!(
                                "Archived dirty worktree for {} at {}",
                                old_assignee,
                                archive_path
                            );
                            task.insert("worktree_archived".into(), Value::String(archive_path));
                        }
                    }
                }
                Err(e) if !force_reassign => {
                    return Err(format!(
                    "Cannot reassign task {task_id}: worktree state check failed for '{old_assignee}': {e} \
                     Use force_reassign=true to proceed anyway."
                ));
                }
                Err(_) => {}
            }
        }
        Ok(None) => {}
        Err(e) if !force_reassign => {
            return Err(format!(
                "Cannot reassign task {task_id}: failed to locate worktree for '{old_assignee}': {e} \
                 Use force_reassign=true to proceed anyway."
            ));
        }
        Err(_) => {}
    }
    Ok(())
}
