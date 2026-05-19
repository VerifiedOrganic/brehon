//! Task actions tool for MCP.
//!
//! Action-based tool for creating, listing, claiming, closing, and updating tasks.
//! Tasks are persisted as JSON files in `.brehon/runtime/tasks/{task_id}.json`.
//!
//! Epics and subtasks: create an epic with `task_type=epic`, then create subtasks
//! with `parent_id=<epic_id>`. When all subtasks of an epic are closed, the close
//! response includes `epic_complete: true` so the supervisor knows.
//!
//! Epic integration flow:
//! - Implementation epics default to an `integration_branch` (a branch where subtask commits collect)
//! - Feature epics also own an `integration_worktree` (a dedicated worktree where that branch lives)
//! - Subtasks have `merge_target` (the branch they merge into, typically the epic's integration_branch)
//! - Subtasks have `integration_status` tracking whether they've been integrated
//! - Plain epics may still exist, but merge subtasks under them must opt in with `direct_to_main=true`

mod action_abort_integration;
mod action_close;
mod action_create;
mod action_followup;
mod action_integrate;
mod action_query;
mod action_update;
mod build_artifact_cleanup;
mod dependencies;
mod epic;
mod final_hardening;
mod followups;
mod git_ops;
mod integration_proof;
mod integration_state;
mod lifecycle;
mod locking;
mod migration;
mod paths;
mod persistence;
mod proof;
mod review_bridge;
mod review_gate;
mod structured_spec;
mod tool;
mod worker_recycle;

// --- Public re-exports (items that were pub or pub(crate) in the original flat module) ---

pub use review_bridge::update_task_status_atomic;
pub use tool::TaskActionsTool;

pub use final_hardening::{FINAL_HARDENING_EPIC_TITLE, FINAL_HARDENING_SEED_TASK_COUNT};

pub(crate) use epic::{
    clear_task_supervisor_integration_conflict, mark_task_supervisor_integration_conflict,
    task_has_integration_conflict_recovery_marker, task_has_supervisor_integration_conflict,
};
pub(crate) use followups::append_task_review_followups;
pub(crate) use integration_state::task_has_active_integration;
pub(crate) use lifecycle::{
    task_has_active_or_unconsolidated_review, unmet_dependency_ids_for_task,
};
pub(crate) use locking::acquire_task_lock;
pub(crate) use review_bridge::{
    release_task_worker_to_review, restore_task_worker_from_review_owner, set_task_review_feedback,
};
pub(crate) use structured_spec::control_plane_scope_issue_for_task;
pub(crate) use worker_recycle::{
    enqueue_worker_session_recycle_surfacing, terminal_worker_recycle_candidate,
};

#[cfg(test)]
#[path = "action_update_tests.rs"]
mod action_update_tests;

#[cfg(test)]
#[path = "integration_proof_tests.rs"]
mod integration_proof_tests;
