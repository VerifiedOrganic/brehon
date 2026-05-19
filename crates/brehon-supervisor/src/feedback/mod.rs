//! Supervisor feedback hardening (Phase 6).
//!
//! This module provides pure planning subsystems plus a durable lifecycle
//! boundary that together implement the feedback loop:
//!
//! - [`trigger`]: detect feedback triggers from a span of durable events.
//! - [`brief`]: build bounded, deterministic feedback briefs.
//! - [`outcome`]: validate supervisor outcomes against policy.
//! - [`apply`]: plan effects through existing Brehon authorities.
//! - [`lifecycle`]: record durable feedback events and invoke an explicit
//!   effect executor.
//!
//! The detector, brief builder, validator, and apply planner are pure functions
//! over inputs. Mutating effects cross the explicit lifecycle executor boundary
//! so feedback never introduces a second authority.

pub mod apply;
pub mod brief;
pub mod cache;
pub mod integration_conflict;
pub mod lifecycle;
pub mod outcome;
pub mod trigger;
pub mod worker_failure;

pub use cache::write_feedback_cache;

pub use apply::{
    is_fold_into_rework, outcome_targets_reviewer_followup, plan_application, ApplyPlan,
};
pub use brief::{build_brief, BriefBuildInput, BriefSourceContext};
pub use integration_conflict::{
    conflict_dedup_key, plan_integration_conflict, ConflictEscalateCause, ConflictResolutionPlan,
};
pub use lifecycle::{
    record_brief_built, record_detected_triggers, record_turn_started,
    record_validate_and_apply_outcome, EventOnlyFeedbackActionExecutor, FeedbackActionExecutor,
    FeedbackLifecycleResult,
};
pub use outcome::{validate_outcome, OutcomeValidation, ValidatedOutcome};
pub use trigger::{
    dedup_triggers, detect_triggers, FeedbackTriggerDetectorInput, NudgeSnapshot,
    PermissionSnapshot, ReviewerFollowupSnapshot, RunActivitySnapshot, TriggerDetectorPolicy,
};
pub use worker_failure::{plan_worker_failure_recovery, EscalateCause, WorkerFailureRecovery};
