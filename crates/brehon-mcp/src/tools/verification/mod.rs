//! Verification tool for MCP.
//!
//! Implements the review coordination system from ARCHITECTURE.md Section 7:
//! - `request_review`: supervisor initiates multi-reviewer panel review
//! - `submit_review`: reviewer submits score + verdict + findings
//! - `review_status`: check review progress
//! - `override`: supervisor overrides review outcome
//! - `reset_rounds`: supervisor resets an exhausted review cycle
//!
//! Uses `brehon-review` crate for scoring, threshold evaluation, and feedback
//! consolidation. File-based state in `.brehon/runtime/reviews/` bridges MCP
//! persistence with the typed domain model.

mod actions;
mod commits;
mod helpers;
mod maintenance;
mod notifications;
mod panel;
mod proof;
#[cfg(test)]
mod proof_tests;
mod review_prompt;
mod scoring;
mod state;
mod tasks;
mod tool;

// --- Public re-exports (items that were pub or pub(crate) in the original flat module) ---

pub use maintenance::ReviewMaintenanceAction;
pub use panel::{PanelLeaseMember, PanelLeaseState, PanelReviewerReplacement};
pub use state::{
    verdict_str, ConsolidatedReport, ReviewRequestFile, ReviewState, StoredCalibration,
    StoredCalibrationEntry, StoredFinding, StoredSubmission,
};
pub use tool::VerificationTool;

pub(crate) use helpers::commits_refer_to_same_oid;
pub(crate) use panel::{find_panel_lease_by_task, release_panel_lease_for_task};
pub(crate) use state::{
    clear_obsolete_review_state_for_resumed_work, delete_review_state, read_review_state,
    read_round_request, reviewed_commits, write_review_state,
};
