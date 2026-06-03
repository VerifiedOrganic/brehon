//! Orchestrator crate for the Brehon system.
//!
//! The orchestrator manages:
//! - Task board (kanban-style state management)
//! - Dependency graph (DAG with cycle detection)
//! - Worker pool (min/max counts, spawning, respawn on death)
//! - Task assignment (dispatch to workers)
//! - Task lifecycle state machine

pub mod assignment;
pub mod continuation;
pub mod dependency_graph;
pub mod error;
pub mod orchestrator;
mod orchestrator_continuation;
mod orchestrator_reconciliation;
pub mod reconciler;
pub mod retry;
pub mod task_board;
pub mod task_lifecycle;
pub mod worker_pool;

#[cfg(test)]
mod orchestrator_continuation_tests;
#[cfg(test)]
mod orchestrator_reconciliation_tests;
#[cfg(test)]
mod reconciler_tests;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod worker_pool_tests;

pub use assignment::AssignmentEngine;
pub use continuation::{
    decide_continuation, ContinuationDecision, ContinuationInput, ContinuationPolicy,
    ContinuationReviewState, ContinuationSessionHealth,
};
pub use dependency_graph::DependencyGraph;
pub use error::{OrchestratorError, Result};
pub use orchestrator::{Orchestrator, OrchestratorConfig, OrchestratorDeps};
pub use reconciler::{
    GitOpsSnapshot, Reconciler, ReconcilerConfig, ReconciliationEscalation, ReconciliationInput,
    ReconciliationPlan, RunFailureAction, RunReleaseAction, RunRenewalAction, RunRepairReason,
    RunRetryAttemptAction, TaskRepairReason, TaskUpdateAction, WorkerCleanupAction,
    WorkerCleanupReason, WorkerSnapshotEntry,
};
pub use retry::{
    decide_retry, RetryDecision, RetryFailureKind, RetryInput, RetryOperatorOverride,
    RetryPermissionState, RetryPolicy, RetryReviewStatus,
};
pub use task_board::{TaskBoard, TaskBoardStats, TaskEntry};
pub use task_lifecycle::{TaskLifecycle, Transition};
pub use worker_pool::{WorkerId, WorkerInfo, WorkerKind, WorkerPool, WorkerPoolConfig};
