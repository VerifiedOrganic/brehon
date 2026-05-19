//! Supervisor feedback domain types (Phase 6).
//!
//! Feedback is Brehon's auditable loop around supervisor judgment:
//!
//! - A `FeedbackTrigger` is detected from the durable event stream when
//!   reviewer follow-ups, worker failures, integration conflicts, or
//!   permission/close blockers need supervisor judgment.
//! - A `FeedbackBrief` snapshots bounded task/run/proof/review context so
//!   the supervisor can adjudicate without unbounded log reading.
//! - A `FeedbackOutcome` records what the supervisor decided.
//! - A `FeedbackDecision` is the validated, durable record of that outcome
//!   plus the brief it answered.
//! - A `FeedbackPolicy` lists which outcomes are allowed, plus rationale
//!   minimums for mutating outcomes.
//!
//! Feedback never bypasses review/integration/permission gates. Mutating
//! outcomes (rework, retry, promote follow-up, queue conflict resolution)
//! must use existing Brehon MCP/orchestrator paths; this module only
//! describes the durable contract.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use crate::event::EventId;
use crate::review::ReviewId;
use crate::run::RunId;
use crate::task::TaskId;

/// Unique identifier for a feedback trigger.
///
/// Trigger ids are derived from a stable key so replaying the same event
/// range never produces duplicate triggers — see `FeedbackTrigger::dedup_key`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct FeedbackTriggerId(pub String);

impl FeedbackTriggerId {
    /// Create a `FeedbackTriggerId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FeedbackTriggerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for FeedbackTriggerId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            Err("feedback trigger id cannot be empty")
        } else {
            Ok(Self::new(trimmed))
        }
    }
}

/// Unique identifier for a feedback adjudication turn.
///
/// A turn pairs one brief with one outcome. The id is regenerated for
/// each new brief so stale outcomes from a prior turn can be rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct FeedbackTurnId(pub String);

impl FeedbackTurnId {
    /// Create a `FeedbackTurnId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FeedbackTurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for FeedbackTurnId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            Err("feedback turn id cannot be empty")
        } else {
            Ok(Self::new(trimmed))
        }
    }
}

/// Kinds of feedback trigger the detector can produce.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackTriggerKind {
    /// Reviewer raised an open follow-up that needs adjudication.
    ReviewerFollowup,
    /// Review consolidated as changes requested.
    ReviewChangesRequested,
    /// A worker run failed and needs a recovery decision.
    WorkerFailed,
    /// A worker has not made progress and a nudge timed out.
    WorkerStuck,
    /// Worker reported a blocker that needs supervisor decision.
    WorkerBlocked,
    /// A permission request is pending or denied.
    PermissionBlocked,
    /// Integration encountered conflicts requiring conflict-resolution work.
    IntegrationConflict,
    /// Task close is blocked (e.g., open follow-ups on container).
    CloseGateBlocked,
    /// A run claim is stale and needs adjudication before recovery.
    StaleClaim,
}

impl FeedbackTriggerKind {
    /// Stable lower-snake identifier used in dedup keys and on-disk fields.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReviewerFollowup => "reviewer_followup",
            Self::ReviewChangesRequested => "review_changes_requested",
            Self::WorkerFailed => "worker_failed",
            Self::WorkerStuck => "worker_stuck",
            Self::WorkerBlocked => "worker_blocked",
            Self::PermissionBlocked => "permission_blocked",
            Self::IntegrationConflict => "integration_conflict",
            Self::CloseGateBlocked => "close_gate_blocked",
            Self::StaleClaim => "stale_claim",
        }
    }
}

impl fmt::Display for FeedbackTriggerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single feedback trigger derived from the durable event stream.
///
/// The detector deduplicates triggers by `dedup_key`. Two trigger
/// instances with the same key are the same trigger across replays.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackTrigger {
    /// Stable trigger identifier.
    pub trigger_id: FeedbackTriggerId,
    /// What kind of trigger this is.
    pub kind: FeedbackTriggerKind,
    /// Task the trigger belongs to, when applicable.
    pub task_id: Option<TaskId>,
    /// Run the trigger belongs to, when applicable.
    pub run_id: Option<RunId>,
    /// Review the trigger belongs to, when applicable.
    pub review_id: Option<ReviewId>,
    /// Source event ids that produced the trigger, lowest first.
    pub source_event_ids: Vec<EventId>,
    /// Bounding event id range scanned by the detector that produced this
    /// trigger. Pairs as `(low, high)` inclusive on both ends.
    pub covered_event_range: Option<(EventId, EventId)>,
    /// Short one-line summary used to render the trigger in lists.
    pub summary: String,
    /// Optional structured payload (free-form). Keep small.
    pub payload: serde_json::Value,
    /// When the trigger was first detected.
    pub created_at: DateTime<Utc>,
}

impl FeedbackTrigger {
    /// Compute the stable dedup key for this trigger.
    ///
    /// Two triggers with the same kind, task, run, review, and optional
    /// payload-specific scope collapse into the same logical trigger.
    /// Reviewer follow-up triggers include `payload.followup_id` so distinct
    /// open follow-ups on the same review cannot hide each other.
    pub fn dedup_key(&self) -> String {
        let task = self.task_id.as_ref().map(|id| id.as_str()).unwrap_or("-");
        let run = self.run_id.as_ref().map(|id| id.as_str()).unwrap_or("-");
        let review = self.review_id.as_ref().map(|id| id.as_str()).unwrap_or("-");
        let scope = self
            .dedup_payload_scope()
            .unwrap_or_else(|| "-".to_string());
        format!("{}|{task}|{run}|{review}|{scope}", self.kind.as_str())
    }

    /// Extra stable dedup scope derived from structured payload fields.
    pub fn dedup_payload_scope(&self) -> Option<String> {
        match self.kind {
            FeedbackTriggerKind::ReviewerFollowup => self
                .payload
                .get("followup_id")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("followup:{value}")),
            _ => None,
        }
    }
}

/// Named section of a feedback brief.
///
/// Briefs are deterministic so two detectors over the same event range
/// produce the same brief sections in the same order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackBriefSection {
    /// Short heading (e.g., `"task"`, `"proof"`, `"recent_events"`).
    pub heading: String,
    /// Body text for the section. Already bounded by the brief builder.
    pub body: String,
    /// True when the section body was truncated to fit the byte cap.
    pub truncated: bool,
    /// True when the section context was unavailable (e.g., missing
    /// proof bundle or store). Surfaces explicitly instead of hiding.
    pub missing: bool,
}

impl FeedbackBriefSection {
    /// Create a fully-populated section.
    pub fn present(heading: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            heading: heading.into(),
            body: body.into(),
            truncated: false,
            missing: false,
        }
    }

    /// Create a "missing" section that explicitly records absence.
    pub fn missing(heading: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            heading: heading.into(),
            body: body.into(),
            truncated: false,
            missing: true,
        }
    }
}

/// Bounded, deterministic brief built for one feedback trigger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackBrief {
    /// Brief's logical turn id; outcomes must echo this back.
    pub turn_id: FeedbackTurnId,
    /// Contract version. Always `FEEDBACK_CONTRACT_VERSION` at build time.
    pub contract_version: u32,
    /// Trigger this brief answers.
    pub trigger: FeedbackTrigger,
    /// Bounded sections in render order.
    pub sections: Vec<FeedbackBriefSection>,
    /// Total bytes of section bodies, after truncation.
    pub total_bytes: usize,
    /// True when at least one section was truncated.
    pub truncated: bool,
    /// True when at least one section was missing.
    pub has_missing_context: bool,
    /// Allowed outcome kinds for this brief, snapshot from policy.
    pub allowed_outcomes: BTreeSet<FeedbackOutcomeKind>,
    /// Maximum rationale length (chars) accepted on mutating outcomes.
    pub rationale_max_chars: usize,
    /// Minimum rationale length (chars) required on mutating outcomes.
    pub rationale_min_chars: usize,
    /// When the brief was built.
    pub built_at: DateTime<Utc>,
}

/// Current feedback contract version. Bump when brief or outcome shape
/// changes in a backward-incompatible way.
pub const FEEDBACK_CONTRACT_VERSION: u32 = 1;

/// Kinds of feedback outcome the supervisor can record.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackOutcomeKind {
    /// Supervisor explicitly recorded no action.
    NoAction,
    /// Send a nudge to the worker through existing nudge path.
    NudgeWorker,
    /// Request rework via existing review/follow-up paths.
    RequestRework,
    /// Retry a failed run via existing retry policy.
    RetryRun,
    /// Promote a reviewer follow-up to a normal Brehon task.
    PromoteReviewerFollowup,
    /// Waive a reviewer follow-up with explicit rationale.
    WaiveReviewerFollowup,
    /// Ask the operator for clarification before further action.
    RequestOperatorClarification,
    /// Queue conflict-resolution work for an integration conflict.
    QueueConflictResolution,
    /// Request a non-destructive integration repair path.
    RequestIntegrationRepair,
    /// Enter drain mode (no new work).
    EnterDrain,
    /// Enter safe mode (escalation surface ready, mutations suspended).
    EnterSafeMode,
    /// Escalate to a human.
    Escalate,
}

impl FeedbackOutcomeKind {
    /// Stable lower-snake identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoAction => "no_action",
            Self::NudgeWorker => "nudge_worker",
            Self::RequestRework => "request_rework",
            Self::RetryRun => "retry_run",
            Self::PromoteReviewerFollowup => "promote_reviewer_followup",
            Self::WaiveReviewerFollowup => "waive_reviewer_followup",
            Self::RequestOperatorClarification => "request_operator_clarification",
            Self::QueueConflictResolution => "queue_conflict_resolution",
            Self::RequestIntegrationRepair => "request_integration_repair",
            Self::EnterDrain => "enter_drain",
            Self::EnterSafeMode => "enter_safe_mode",
            Self::Escalate => "escalate",
        }
    }

    /// Mutating outcomes change task/queue/run state and require a
    /// non-empty rationale to be recorded.
    pub fn is_mutating(self) -> bool {
        matches!(
            self,
            Self::RequestRework
                | Self::RetryRun
                | Self::PromoteReviewerFollowup
                | Self::WaiveReviewerFollowup
                | Self::QueueConflictResolution
                | Self::RequestIntegrationRepair
                | Self::EnterDrain
                | Self::EnterSafeMode
        )
    }

    /// All outcome kinds, useful when constructing a maximal allow-list.
    pub fn all() -> &'static [FeedbackOutcomeKind] {
        &[
            Self::NoAction,
            Self::NudgeWorker,
            Self::RequestRework,
            Self::RetryRun,
            Self::PromoteReviewerFollowup,
            Self::WaiveReviewerFollowup,
            Self::RequestOperatorClarification,
            Self::QueueConflictResolution,
            Self::RequestIntegrationRepair,
            Self::EnterDrain,
            Self::EnterSafeMode,
            Self::Escalate,
        ]
    }
}

impl fmt::Display for FeedbackOutcomeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome submitted by the supervisor for a feedback turn.
///
/// Outcomes are untrusted until validated. The validator (P6.5) decides
/// whether to record a `FeedbackDecision` or a rejection event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackOutcome {
    /// Contract version the supervisor responded against.
    pub contract_version: u32,
    /// Turn id from the brief this outcome answers.
    pub turn_id: FeedbackTurnId,
    /// Trigger id from the brief.
    pub trigger_id: FeedbackTriggerId,
    /// Outcome kind.
    pub kind: FeedbackOutcomeKind,
    /// Supervisor rationale. Required (non-empty) for mutating outcomes.
    pub rationale: String,
    /// Optional follow-up id when the outcome targets a specific follow-up.
    pub followup_id: Option<String>,
    /// Optional task id when the outcome creates work on a specific task.
    pub task_id: Option<TaskId>,
    /// Optional run id when the outcome targets a specific run.
    pub run_id: Option<RunId>,
    /// Optional supervisor agent name for attribution.
    pub supervisor_id: Option<String>,
    /// Optional free-form structured payload (e.g., target branch).
    pub payload: serde_json::Value,
}

/// Durable feedback decision: a validated outcome paired with the brief
/// it answered. Stored as proof + emitted as a `FeedbackDecisionRecorded`
/// event so the loop is fully auditable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackDecision {
    /// Brief that produced this decision.
    pub brief: FeedbackBrief,
    /// Outcome the supervisor recorded.
    pub outcome: FeedbackOutcome,
    /// When the decision was recorded.
    pub decided_at: DateTime<Utc>,
}

/// Reason a feedback outcome was rejected by the validator.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackOutcomeRejectionReason {
    /// Contract version mismatch.
    ContractVersionMismatch,
    /// Outcome turn id did not match the brief turn id.
    TurnIdMismatch,
    /// Outcome trigger id did not match the brief trigger id.
    TriggerIdMismatch,
    /// Outcome kind is not in the policy/brief allow-list.
    OutcomeKindDenied,
    /// Outcome kind is not valid for the trigger kind.
    TriggerKindMismatch,
    /// Mutating outcome missing required rationale.
    MissingRationale,
    /// Mutating outcome missing a required field (e.g., followup_id).
    MissingRequiredField,
    /// Rationale exceeded the policy character cap.
    RationaleTooLong,
    /// Unknown / unrecognized outcome shape.
    Unknown,
}

impl FeedbackOutcomeRejectionReason {
    /// Stable lower-snake identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContractVersionMismatch => "contract_version_mismatch",
            Self::TurnIdMismatch => "turn_id_mismatch",
            Self::TriggerIdMismatch => "trigger_id_mismatch",
            Self::OutcomeKindDenied => "outcome_kind_denied",
            Self::TriggerKindMismatch => "trigger_kind_mismatch",
            Self::MissingRationale => "missing_rationale",
            Self::MissingRequiredField => "missing_required_field",
            Self::RationaleTooLong => "rationale_too_long",
            Self::Unknown => "unknown",
        }
    }
}

/// Compact, bounded view of supervisor feedback activity for a task.
///
/// Mirrors the proof-side `ProofSummary` pattern: durable events plus an
/// optional side-channel cache that the TUI can read without depending
/// on the fjall projection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FeedbackTaskSummary {
    /// Active (open) triggers for this task.
    pub active_triggers: Vec<FeedbackTriggerSummary>,
    /// Recently recorded decisions, newest first.
    pub recent_decisions: Vec<FeedbackDecisionSummary>,
    /// Pending operator clarification requests.
    pub pending_clarifications: Vec<FeedbackClarificationSummary>,
    /// Visible escalations from feedback (e.g., retry-exhausted).
    pub escalations: Vec<FeedbackEscalationSummary>,
    /// True when the system has been put into drain mode by feedback.
    pub drain_active: bool,
    /// True when the system has been put into safe mode by feedback.
    pub safe_mode_active: bool,
    /// Optional cache write timestamp (RFC 3339).
    pub updated_at: Option<String>,
}

/// Compact trigger row for the TUI feedback panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackTriggerSummary {
    pub trigger_id: String,
    pub kind: String,
    pub summary: String,
    pub created_at: String,
}

/// Compact decision row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackDecisionSummary {
    pub trigger_id: String,
    pub turn_id: String,
    pub outcome_kind: String,
    pub summary: String,
    pub decided_at: String,
}

/// Compact clarification request row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackClarificationSummary {
    pub trigger_id: String,
    pub question: String,
    pub requested_at: String,
}

/// Compact escalation row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackEscalationSummary {
    pub trigger_id: String,
    pub reason: String,
    pub raised_at: String,
}

/// Policy controlling which feedback outcomes are allowed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackPolicy {
    /// Allowed outcome kinds. Outcomes outside the set are rejected.
    pub allowed_outcomes: BTreeSet<FeedbackOutcomeKind>,
    /// Minimum rationale length in chars for mutating outcomes.
    pub rationale_min_chars: usize,
    /// Maximum rationale length in chars for any outcome.
    pub rationale_max_chars: usize,
    /// Maximum bytes a brief may consume across all section bodies.
    pub max_brief_bytes: usize,
    /// True when the policy permits drain mode.
    pub allow_drain: bool,
    /// True when the policy permits safe mode.
    pub allow_safe_mode: bool,
}

impl FeedbackPolicy {
    /// Default conservative policy. All outcomes allowed except drain
    /// and safe mode (must be explicitly enabled per deployment).
    pub fn conservative() -> Self {
        let mut allowed: BTreeSet<FeedbackOutcomeKind> =
            FeedbackOutcomeKind::all().iter().copied().collect();
        allowed.remove(&FeedbackOutcomeKind::EnterDrain);
        allowed.remove(&FeedbackOutcomeKind::EnterSafeMode);
        Self {
            allowed_outcomes: allowed,
            rationale_min_chars: 8,
            rationale_max_chars: 4_000,
            max_brief_bytes: 16 * 1024,
            allow_drain: false,
            allow_safe_mode: false,
        }
    }

    /// Returns true when the policy allows the given outcome kind.
    pub fn allows(&self, kind: FeedbackOutcomeKind) -> bool {
        if !self.allowed_outcomes.contains(&kind) {
            return false;
        }
        match kind {
            FeedbackOutcomeKind::EnterDrain => self.allow_drain,
            FeedbackOutcomeKind::EnterSafeMode => self.allow_safe_mode,
            _ => true,
        }
    }
}

impl Default for FeedbackPolicy {
    fn default() -> Self {
        Self::conservative()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_id_round_trips_through_str_and_serde() {
        let id: FeedbackTriggerId = "fb-trig-1".parse().unwrap();
        assert_eq!(id.as_str(), "fb-trig-1");
        assert!("   ".parse::<FeedbackTriggerId>().is_err());
        let payload = serde_json::to_string(&id).unwrap();
        let parsed: FeedbackTriggerId = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn turn_id_rejects_blank_input() {
        assert!("".parse::<FeedbackTurnId>().is_err());
        assert!(" \n ".parse::<FeedbackTurnId>().is_err());
        let id: FeedbackTurnId = "turn-x".parse().unwrap();
        assert_eq!(id.as_str(), "turn-x");
    }

    #[test]
    fn trigger_dedup_key_groups_by_kind_task_run_review_and_followup() {
        let trigger = FeedbackTrigger {
            trigger_id: FeedbackTriggerId::new("ignored"),
            kind: FeedbackTriggerKind::ReviewerFollowup,
            task_id: Some(TaskId::new("T-1")),
            run_id: None,
            review_id: Some(ReviewId::new("REV-1")),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            summary: "first rendering".to_string(),
            payload: serde_json::json!({"followup_id":"FUP-1", "k":"v1"}),
            created_at: Utc::now(),
        };
        let mut duplicate = trigger.clone();
        duplicate.trigger_id = FeedbackTriggerId::new("other");
        duplicate.summary = "different wording".to_string();
        duplicate.payload = serde_json::json!({"followup_id":"FUP-1", "k":"v2"});
        assert_eq!(trigger.dedup_key(), duplicate.dedup_key());

        let mut different_followup = trigger.clone();
        different_followup.payload = serde_json::json!({"followup_id":"FUP-2"});
        assert_ne!(trigger.dedup_key(), different_followup.dedup_key());

        let mut different_task = trigger.clone();
        different_task.task_id = Some(TaskId::new("T-2"));
        assert_ne!(trigger.dedup_key(), different_task.dedup_key());

        let mut different_kind = trigger.clone();
        different_kind.kind = FeedbackTriggerKind::WorkerStuck;
        assert_ne!(trigger.dedup_key(), different_kind.dedup_key());
    }

    #[test]
    fn outcome_kind_string_round_trips() {
        for kind in FeedbackOutcomeKind::all() {
            let value = serde_json::to_value(kind).unwrap();
            let back: FeedbackOutcomeKind = serde_json::from_value(value).unwrap();
            assert_eq!(*kind, back);
            assert!(!kind.as_str().is_empty());
        }
    }

    #[test]
    fn mutating_outcomes_are_correctly_classified() {
        for kind in FeedbackOutcomeKind::all() {
            match kind {
                FeedbackOutcomeKind::NoAction
                | FeedbackOutcomeKind::NudgeWorker
                | FeedbackOutcomeKind::RequestOperatorClarification
                | FeedbackOutcomeKind::Escalate => {
                    assert!(!kind.is_mutating(), "{kind} should not mutate");
                }
                _ => assert!(kind.is_mutating(), "{kind} should mutate"),
            }
        }
    }

    #[test]
    fn conservative_policy_denies_drain_and_safe_mode_by_default() {
        let policy = FeedbackPolicy::conservative();
        assert!(policy.allows(FeedbackOutcomeKind::Escalate));
        assert!(policy.allows(FeedbackOutcomeKind::RetryRun));
        assert!(!policy.allows(FeedbackOutcomeKind::EnterDrain));
        assert!(!policy.allows(FeedbackOutcomeKind::EnterSafeMode));
    }

    #[test]
    fn policy_round_trips_through_serde() {
        let mut policy = FeedbackPolicy::conservative();
        // Drain is double-gated: it must be both in the allow-list AND
        // `allow_drain` must be true.
        policy.allow_drain = true;
        policy
            .allowed_outcomes
            .insert(FeedbackOutcomeKind::EnterDrain);
        let payload = serde_json::to_string(&policy).unwrap();
        let parsed: FeedbackPolicy = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed, policy);
        assert!(parsed.allows(FeedbackOutcomeKind::EnterDrain));

        // Removing from allow-list overrides the boolean.
        let mut allowlist_only = parsed.clone();
        allowlist_only
            .allowed_outcomes
            .remove(&FeedbackOutcomeKind::EnterDrain);
        assert!(!allowlist_only.allows(FeedbackOutcomeKind::EnterDrain));
    }

    #[test]
    fn brief_section_constructors_set_missing_flag() {
        let present = FeedbackBriefSection::present("task", "body");
        assert!(!present.missing);
        let missing = FeedbackBriefSection::missing("proof", "(no proof bundle recorded)");
        assert!(missing.missing);
    }
}
