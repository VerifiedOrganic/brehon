//! Feedback outcome validator.
//!
//! `validate_outcome` is a pure function over `(brief, outcome, policy)`.
//! It either returns an `Accepted(ValidatedOutcome)` carrying a durable
//! `FeedbackDecision`, or `Rejected { reason, message }` describing why
//! the outcome was refused. Unknown / unrecognized shapes fail closed.

use chrono::Utc;

use brehon_types::{
    FeedbackBrief, FeedbackDecision, FeedbackOutcome, FeedbackOutcomeKind,
    FeedbackOutcomeRejectionReason, FeedbackPolicy, FEEDBACK_CONTRACT_VERSION,
};

/// Validated outcome paired with the brief it answers.
#[derive(Debug, Clone)]
pub struct ValidatedOutcome {
    pub decision: FeedbackDecision,
}

/// Result of validating a raw outcome.
///
/// `Accepted` is intentionally larger than `Rejected`; this enum is only
/// produced once per feedback round and is consumed immediately, so the
/// size delta has no measurable cost.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum OutcomeValidation {
    Accepted(ValidatedOutcome),
    Rejected {
        reason: FeedbackOutcomeRejectionReason,
        message: String,
    },
}

impl OutcomeValidation {
    /// True when the validator accepted the outcome.
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
}

/// Validate a supervisor outcome.
pub fn validate_outcome(
    brief: &FeedbackBrief,
    outcome: &FeedbackOutcome,
    policy: &FeedbackPolicy,
) -> OutcomeValidation {
    // 1. Contract version must match the brief's version (which itself
    //    equals FEEDBACK_CONTRACT_VERSION at build time).
    if outcome.contract_version != brief.contract_version
        || outcome.contract_version != FEEDBACK_CONTRACT_VERSION
    {
        return reject(
            FeedbackOutcomeRejectionReason::ContractVersionMismatch,
            format!(
                "Outcome contract_version={} but brief expects {} (current FEEDBACK_CONTRACT_VERSION={}).",
                outcome.contract_version, brief.contract_version, FEEDBACK_CONTRACT_VERSION
            ),
        );
    }

    // 2. Turn id and trigger id must match the brief so a stale outcome
    //    from a prior turn cannot be re-applied.
    if outcome.turn_id != brief.turn_id {
        return reject(
            FeedbackOutcomeRejectionReason::TurnIdMismatch,
            format!(
                "Outcome turn_id {} did not match brief turn_id {}",
                outcome.turn_id, brief.turn_id
            ),
        );
    }
    if outcome.trigger_id != brief.trigger.trigger_id {
        return reject(
            FeedbackOutcomeRejectionReason::TriggerIdMismatch,
            format!(
                "Outcome trigger_id {} did not match brief trigger_id {}",
                outcome.trigger_id, brief.trigger.trigger_id
            ),
        );
    }

    // 3. Outcome kind must be in the policy allow-list (with drain/safe
    //    mode double-gated by allow_drain / allow_safe_mode).
    if !policy.allows(outcome.kind) {
        return reject(
            FeedbackOutcomeRejectionReason::OutcomeKindDenied,
            format!(
                "Outcome kind {} is not permitted by current FeedbackPolicy.",
                outcome.kind.as_str()
            ),
        );
    }
    if !brief.allowed_outcomes.contains(&outcome.kind) {
        return reject(
            FeedbackOutcomeRejectionReason::OutcomeKindDenied,
            format!(
                "Outcome kind {} is not in the brief's allow-list.",
                outcome.kind.as_str()
            ),
        );
    }
    if !outcome_allowed_for_trigger(brief.trigger.kind, outcome.kind) {
        return reject(
            FeedbackOutcomeRejectionReason::TriggerKindMismatch,
            format!(
                "Outcome kind {} is not valid for trigger kind {}.",
                outcome.kind.as_str(),
                brief.trigger.kind.as_str()
            ),
        );
    }

    // 4. Rationale length checks.
    let rationale_chars = outcome.rationale.chars().count();
    if rationale_chars > policy.rationale_max_chars {
        return reject(
            FeedbackOutcomeRejectionReason::RationaleTooLong,
            format!(
                "Rationale length {} exceeds policy max {}.",
                rationale_chars, policy.rationale_max_chars
            ),
        );
    }
    if outcome.kind.is_mutating() && rationale_chars < policy.rationale_min_chars {
        return reject(
            FeedbackOutcomeRejectionReason::MissingRationale,
            format!(
                "Mutating outcome {} requires rationale of at least {} chars (got {}).",
                outcome.kind.as_str(),
                policy.rationale_min_chars,
                rationale_chars
            ),
        );
    }

    // 5. Mutating outcome required-field checks.
    if let Some(reason) = required_fields_missing(outcome) {
        return reject(FeedbackOutcomeRejectionReason::MissingRequiredField, reason);
    }

    let summary = decision_summary(outcome);
    let decision = FeedbackDecision {
        brief: brief.clone(),
        outcome: outcome.clone(),
        decided_at: Utc::now(),
    };
    OutcomeValidation::Accepted(ValidatedOutcome { decision }).with_summary_for_application(summary)
}

/// Helper trait to attach an application summary string to an accepted
/// outcome. The summary is what downstream apply paths use as their
/// recorded `application_summary` for `FeedbackDecisionRecorded` events.
impl OutcomeValidation {
    /// Override the decision summary, used by callers to record what the
    /// outcome will produce on application.
    fn with_summary_for_application(self, _summary: String) -> Self {
        // The summary is currently not stored on FeedbackDecision; the
        // apply paths derive it from outcome.kind and supporting fields.
        // The argument is reserved so the contract stays stable when we
        // later expose summary on the decision record.
        self
    }
}

fn required_fields_missing(outcome: &FeedbackOutcome) -> Option<String> {
    match outcome.kind {
        FeedbackOutcomeKind::PromoteReviewerFollowup
        | FeedbackOutcomeKind::WaiveReviewerFollowup
            if outcome
                .followup_id
                .as_deref()
                .map(str::trim)
                .map(str::is_empty)
                .unwrap_or(true) =>
        {
            return Some(format!(
                "Outcome kind {} requires a non-empty followup_id.",
                outcome.kind.as_str()
            ));
        }
        FeedbackOutcomeKind::RetryRun if outcome.run_id.is_none() && outcome.task_id.is_none() => {
            return Some("Outcome kind retry_run requires either run_id or task_id.".to_string());
        }
        FeedbackOutcomeKind::RequestRework if outcome.task_id.is_none() => {
            return Some("Outcome kind request_rework requires task_id.".to_string());
        }
        FeedbackOutcomeKind::QueueConflictResolution
        | FeedbackOutcomeKind::RequestIntegrationRepair
            if outcome.task_id.is_none() =>
        {
            return Some(format!(
                "Outcome kind {} requires task_id of the conflicting task.",
                outcome.kind.as_str()
            ));
        }
        _ => {}
    }
    None
}

fn outcome_allowed_for_trigger(
    trigger_kind: brehon_types::FeedbackTriggerKind,
    outcome_kind: FeedbackOutcomeKind,
) -> bool {
    use brehon_types::FeedbackTriggerKind as Trigger;
    use FeedbackOutcomeKind as Outcome;

    match outcome_kind {
        Outcome::NoAction
        | Outcome::RequestOperatorClarification
        | Outcome::Escalate
        | Outcome::EnterDrain
        | Outcome::EnterSafeMode => true,
        Outcome::PromoteReviewerFollowup | Outcome::WaiveReviewerFollowup => {
            matches!(trigger_kind, Trigger::ReviewerFollowup)
        }
        Outcome::RequestRework => matches!(
            trigger_kind,
            Trigger::ReviewerFollowup
                | Trigger::ReviewChangesRequested
                | Trigger::WorkerFailed
                | Trigger::WorkerStuck
                | Trigger::WorkerBlocked
        ),
        Outcome::RetryRun => matches!(
            trigger_kind,
            Trigger::WorkerFailed | Trigger::WorkerStuck | Trigger::StaleClaim
        ),
        Outcome::NudgeWorker => matches!(
            trigger_kind,
            Trigger::WorkerFailed
                | Trigger::WorkerStuck
                | Trigger::WorkerBlocked
                | Trigger::PermissionBlocked
                | Trigger::StaleClaim
        ),
        Outcome::QueueConflictResolution | Outcome::RequestIntegrationRepair => {
            matches!(trigger_kind, Trigger::IntegrationConflict)
        }
    }
}

fn decision_summary(outcome: &FeedbackOutcome) -> String {
    let mut parts: Vec<String> = vec![outcome.kind.as_str().to_string()];
    if let Some(task) = outcome.task_id.as_ref() {
        parts.push(format!("task={}", task.as_str()));
    }
    if let Some(run) = outcome.run_id.as_ref() {
        parts.push(format!("run={}", run.as_str()));
    }
    if let Some(followup) = outcome.followup_id.as_ref() {
        parts.push(format!("followup={followup}"));
    }
    parts.join(" ")
}

fn reject(reason: FeedbackOutcomeRejectionReason, message: String) -> OutcomeValidation {
    OutcomeValidation::Rejected { reason, message }
}

#[cfg(test)]
mod feedback_outcome_tests {
    use super::*;
    use brehon_types::{
        EventId, FeedbackBriefSection, FeedbackTrigger, FeedbackTriggerId, FeedbackTriggerKind,
        FeedbackTurnId, TaskId,
    };
    use std::collections::BTreeSet;

    fn make_brief(allowed: &[FeedbackOutcomeKind]) -> FeedbackBrief {
        let trigger = FeedbackTrigger {
            trigger_id: FeedbackTriggerId::new("fb-out-1"),
            kind: FeedbackTriggerKind::ReviewerFollowup,
            task_id: Some(TaskId::new("T-out")),
            run_id: None,
            review_id: None,
            source_event_ids: vec![EventId::new(1)],
            covered_event_range: Some((EventId::new(1), EventId::new(1))),
            summary: "follow-up".into(),
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        let mut allowed_set: BTreeSet<FeedbackOutcomeKind> = BTreeSet::new();
        for kind in allowed {
            allowed_set.insert(*kind);
        }
        FeedbackBrief {
            turn_id: FeedbackTurnId::new("turn-1"),
            contract_version: FEEDBACK_CONTRACT_VERSION,
            trigger,
            sections: vec![FeedbackBriefSection::present("policy", "")],
            total_bytes: 0,
            truncated: false,
            has_missing_context: false,
            allowed_outcomes: allowed_set,
            rationale_max_chars: 256,
            rationale_min_chars: 8,
            built_at: chrono::Utc::now(),
        }
    }

    fn base_outcome(brief: &FeedbackBrief, kind: FeedbackOutcomeKind) -> FeedbackOutcome {
        FeedbackOutcome {
            contract_version: FEEDBACK_CONTRACT_VERSION,
            turn_id: brief.turn_id.clone(),
            trigger_id: brief.trigger.trigger_id.clone(),
            kind,
            rationale: "rationale provided".into(),
            followup_id: None,
            task_id: brief.trigger.task_id.clone(),
            run_id: None,
            supervisor_id: Some("supervisor-1".into()),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn rejects_contract_version_mismatch() {
        let brief = make_brief(&[FeedbackOutcomeKind::PromoteReviewerFollowup]);
        let policy = FeedbackPolicy::conservative();
        let mut outcome = base_outcome(&brief, FeedbackOutcomeKind::PromoteReviewerFollowup);
        outcome.contract_version = FEEDBACK_CONTRACT_VERSION + 1;
        outcome.followup_id = Some("FUP-1".into());
        let result = validate_outcome(&brief, &outcome, &policy);
        match result {
            OutcomeValidation::Rejected { reason, .. } => assert_eq!(
                reason,
                FeedbackOutcomeRejectionReason::ContractVersionMismatch
            ),
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn rejects_turn_id_and_trigger_id_mismatches() {
        let brief = make_brief(&[FeedbackOutcomeKind::PromoteReviewerFollowup]);
        let policy = FeedbackPolicy::conservative();
        let mut outcome = base_outcome(&brief, FeedbackOutcomeKind::PromoteReviewerFollowup);
        outcome.followup_id = Some("FUP-1".into());

        let mut bad_turn = outcome.clone();
        bad_turn.turn_id = FeedbackTurnId::new("turn-other");
        match validate_outcome(&brief, &bad_turn, &policy) {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::TurnIdMismatch)
            }
            _ => panic!("expected rejection for turn mismatch"),
        }

        let mut bad_trigger = outcome.clone();
        bad_trigger.trigger_id = FeedbackTriggerId::new("fb-other");
        match validate_outcome(&brief, &bad_trigger, &policy) {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::TriggerIdMismatch)
            }
            _ => panic!("expected rejection for trigger mismatch"),
        }
    }

    #[test]
    fn unknown_outcome_kind_fails_closed_via_policy_check() {
        // Brief allows only NoAction; supervisor proposes RetryRun.
        let brief = make_brief(&[FeedbackOutcomeKind::NoAction]);
        let policy = FeedbackPolicy::conservative();
        let mut outcome = base_outcome(&brief, FeedbackOutcomeKind::RetryRun);
        outcome.run_id = Some(brehon_types::RunId::new("run-1"));
        let result = validate_outcome(&brief, &outcome, &policy);
        match result {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::OutcomeKindDenied)
            }
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn drain_is_rejected_unless_policy_explicitly_allows_it() {
        let brief = make_brief(&[FeedbackOutcomeKind::EnterDrain]);
        let mut policy = FeedbackPolicy::conservative();
        // Conservative policy excludes drain via the bool gate even if it
        // is in the brief's allow-list.
        let outcome = FeedbackOutcome {
            rationale: "supervisor decided to drain".into(),
            ..base_outcome(&brief, FeedbackOutcomeKind::EnterDrain)
        };
        match validate_outcome(&brief, &outcome, &policy) {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::OutcomeKindDenied)
            }
            _ => panic!("expected drain rejection"),
        }
        policy.allow_drain = true;
        policy
            .allowed_outcomes
            .insert(FeedbackOutcomeKind::EnterDrain);
        match validate_outcome(&brief, &outcome, &policy) {
            OutcomeValidation::Accepted(_) => {}
            other => panic!("expected acceptance after enabling drain, got {other:?}"),
        }
    }

    #[test]
    fn rationale_length_and_required_fields_are_enforced() {
        let brief = make_brief(&[FeedbackOutcomeKind::PromoteReviewerFollowup]);
        let policy = FeedbackPolicy::conservative();

        // Missing rationale on mutating outcome.
        let mut outcome = base_outcome(&brief, FeedbackOutcomeKind::PromoteReviewerFollowup);
        outcome.rationale = "x".into();
        outcome.followup_id = Some("FUP-1".into());
        match validate_outcome(&brief, &outcome, &policy) {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::MissingRationale)
            }
            _ => panic!("expected rejection for short rationale"),
        }

        // Rationale too long.
        let mut outcome = base_outcome(&brief, FeedbackOutcomeKind::PromoteReviewerFollowup);
        outcome.rationale = "y".repeat(policy.rationale_max_chars + 1);
        outcome.followup_id = Some("FUP-1".into());
        match validate_outcome(&brief, &outcome, &policy) {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::RationaleTooLong)
            }
            _ => panic!("expected rejection for over-long rationale"),
        }

        // Missing followup_id.
        let outcome = base_outcome(&brief, FeedbackOutcomeKind::PromoteReviewerFollowup);
        match validate_outcome(&brief, &outcome, &policy) {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::MissingRequiredField)
            }
            _ => panic!("expected rejection for missing followup_id"),
        }
    }

    #[test]
    fn valid_mutating_outcome_is_accepted_and_decision_recorded() {
        let brief = make_brief(&[FeedbackOutcomeKind::PromoteReviewerFollowup]);
        let policy = FeedbackPolicy::conservative();
        let mut outcome = base_outcome(&brief, FeedbackOutcomeKind::PromoteReviewerFollowup);
        outcome.followup_id = Some("FUP-1".into());
        outcome.rationale = "promote per supervisor decision".into();
        match validate_outcome(&brief, &outcome, &policy) {
            OutcomeValidation::Accepted(validated) => {
                assert_eq!(validated.decision.brief.turn_id, brief.turn_id);
                assert_eq!(
                    validated.decision.outcome.kind,
                    FeedbackOutcomeKind::PromoteReviewerFollowup
                );
            }
            other => panic!("expected acceptance, got {other:?}"),
        }
    }

    #[test]
    fn trigger_kind_mismatch_is_rejected_before_application() {
        let mut brief = make_brief(&[FeedbackOutcomeKind::PromoteReviewerFollowup]);
        brief.trigger.kind = FeedbackTriggerKind::WorkerFailed;
        let policy = FeedbackPolicy::conservative();
        let mut outcome = base_outcome(&brief, FeedbackOutcomeKind::PromoteReviewerFollowup);
        outcome.followup_id = Some("FUP-1".into());
        match validate_outcome(&brief, &outcome, &policy) {
            OutcomeValidation::Rejected { reason, .. } => {
                assert_eq!(reason, FeedbackOutcomeRejectionReason::TriggerKindMismatch)
            }
            other => panic!("expected trigger mismatch rejection, got {other:?}"),
        }
    }
}
