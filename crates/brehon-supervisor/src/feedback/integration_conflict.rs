//! Integration-conflict decision helpers.
//!
//! Pure helpers that translate a feedback decision on an
//! `integration_conflict` trigger into a `ConflictResolutionPlan` that
//! describes how the conflict-resolution work should be queued or how
//! integration repair should be requested.
//!
//! Constraints preserved here:
//!
//! - Destructive git operations (`git reset --hard`, force pushes, branch
//!   deletion) remain operator-approved. The plan only describes
//!   non-destructive paths: requesting conflict-resolution work as a
//!   normal Brehon task, or requesting an integration-repair followup.
//! - Integration stays blocked until the conflict path resolves. Plans
//!   carry a stable `dedup_key` so the orchestrator never queues the
//!   same conflict-resolution task twice.

use brehon_types::{FeedbackDecision, FeedbackOutcomeKind, FeedbackTriggerKind};

/// Plan returned by [`plan_integration_conflict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictResolutionPlan {
    /// Create a normal Brehon conflict-resolution task. Downstream callers
    /// use the existing `task action=create` path; this plan never
    /// invokes destructive git operations directly.
    QueueResolutionTask {
        task_id: String,
        dedup_key: String,
        rationale: String,
    },
    /// Ask Brehon's integration repair path to retry the integration with
    /// a clean worktree. Operator approval is still required for any
    /// destructive git action that the repair path proposes.
    RequestRepair {
        task_id: String,
        dedup_key: String,
        rationale: String,
    },
    /// Escalate the conflict to a human operator. Used when supervisor
    /// chose Escalate or when the outcome kind is not supported.
    Escalate {
        task_id: Option<String>,
        rationale: String,
        cause: ConflictEscalateCause,
    },
    /// No downstream effect.
    NoAction,
}

/// Reason an integration-conflict plan produced an escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictEscalateCause {
    /// Supervisor explicitly chose Escalate or RequestOperatorClarification.
    SupervisorChoice,
    /// Outcome kind is not supported on an integration_conflict trigger.
    UnsupportedOutcome,
    /// A mutation plan was missing the id needed to call the existing path.
    MissingRequiredField,
    /// Trigger kind is not actually an integration_conflict.
    TriggerKindMismatch,
}

/// Plan a conflict-resolution path for a validated decision.
pub fn plan_integration_conflict(decision: &FeedbackDecision) -> ConflictResolutionPlan {
    if !matches!(
        decision.brief.trigger.kind,
        FeedbackTriggerKind::IntegrationConflict
    ) {
        return ConflictResolutionPlan::Escalate {
            task_id: decision
                .brief
                .trigger
                .task_id
                .as_ref()
                .map(|id| id.as_str().to_string()),
            rationale: format!(
                "Integration-conflict planner invoked for trigger kind {}.",
                decision.brief.trigger.kind.as_str()
            ),
            cause: ConflictEscalateCause::TriggerKindMismatch,
        };
    }

    let outcome = &decision.outcome;
    let task_id = outcome
        .task_id
        .as_ref()
        .map(|id| id.as_str().to_string())
        .or_else(|| {
            decision
                .brief
                .trigger
                .task_id
                .as_ref()
                .map(|id| id.as_str().to_string())
        });
    match outcome.kind {
        FeedbackOutcomeKind::QueueConflictResolution => {
            let Some(task_id_string) = task_id else {
                return ConflictResolutionPlan::Escalate {
                    task_id: None,
                    rationale: "Outcome kind queue_conflict_resolution requires task_id."
                        .to_string(),
                    cause: ConflictEscalateCause::MissingRequiredField,
                };
            };
            let dedup_key = conflict_dedup_key(&task_id_string);
            ConflictResolutionPlan::QueueResolutionTask {
                task_id: task_id_string,
                dedup_key,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::RequestIntegrationRepair => {
            let Some(task_id_string) = task_id else {
                return ConflictResolutionPlan::Escalate {
                    task_id: None,
                    rationale: "Outcome kind request_integration_repair requires task_id."
                        .to_string(),
                    cause: ConflictEscalateCause::MissingRequiredField,
                };
            };
            let dedup_key = conflict_dedup_key(&task_id_string);
            ConflictResolutionPlan::RequestRepair {
                task_id: task_id_string,
                dedup_key,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::Escalate => ConflictResolutionPlan::Escalate {
            task_id,
            rationale: outcome.rationale.clone(),
            cause: ConflictEscalateCause::SupervisorChoice,
        },
        FeedbackOutcomeKind::RequestOperatorClarification => ConflictResolutionPlan::Escalate {
            task_id,
            rationale: outcome.rationale.clone(),
            cause: ConflictEscalateCause::SupervisorChoice,
        },
        FeedbackOutcomeKind::NoAction => ConflictResolutionPlan::NoAction,
        _ => ConflictResolutionPlan::Escalate {
            task_id,
            rationale: format!(
                "Outcome kind {} is not supported on integration_conflict triggers.",
                outcome.kind.as_str()
            ),
            cause: ConflictEscalateCause::UnsupportedOutcome,
        },
    }
}

/// Stable dedup key used by downstream callers to avoid creating two
/// conflict-resolution tasks for the same source task.
pub fn conflict_dedup_key(task_id: &str) -> String {
    format!("integration_conflict:{task_id}")
}

#[cfg(test)]
mod integration_conflict_tests {
    use super::*;
    use brehon_types::{
        EventId, FeedbackBrief, FeedbackBriefSection, FeedbackOutcome, FeedbackOutcomeKind,
        FeedbackTrigger, FeedbackTriggerId, FeedbackTriggerKind, FeedbackTurnId, TaskId,
        FEEDBACK_CONTRACT_VERSION,
    };
    use std::collections::BTreeSet;

    fn conflict_brief() -> FeedbackBrief {
        let trigger = FeedbackTrigger {
            trigger_id: FeedbackTriggerId::new("fb-ic-1"),
            kind: FeedbackTriggerKind::IntegrationConflict,
            task_id: Some(TaskId::new("T-ic")),
            run_id: None,
            review_id: None,
            source_event_ids: vec![EventId::new(31)],
            covered_event_range: Some((EventId::new(31), EventId::new(31))),
            summary: "Integration conflict".into(),
            payload: serde_json::json!({
                "conflicts": ["src/lib.rs"]
            }),
            created_at: chrono::Utc::now(),
        };
        let mut allowed: BTreeSet<FeedbackOutcomeKind> = BTreeSet::new();
        for kind in FeedbackOutcomeKind::all() {
            allowed.insert(*kind);
        }
        FeedbackBrief {
            turn_id: FeedbackTurnId::new("turn-ic"),
            contract_version: FEEDBACK_CONTRACT_VERSION,
            trigger,
            sections: vec![FeedbackBriefSection::present("trigger", "")],
            total_bytes: 0,
            truncated: false,
            has_missing_context: false,
            allowed_outcomes: allowed,
            rationale_max_chars: 256,
            rationale_min_chars: 8,
            built_at: chrono::Utc::now(),
        }
    }

    fn decision(kind: FeedbackOutcomeKind) -> FeedbackDecision {
        let brief = conflict_brief();
        let outcome = FeedbackOutcome {
            contract_version: FEEDBACK_CONTRACT_VERSION,
            turn_id: brief.turn_id.clone(),
            trigger_id: brief.trigger.trigger_id.clone(),
            kind,
            rationale: "supervisor conflict decision".into(),
            followup_id: None,
            task_id: brief.trigger.task_id.clone(),
            run_id: None,
            supervisor_id: Some("supervisor-1".into()),
            payload: serde_json::json!({}),
        };
        FeedbackDecision {
            brief,
            outcome,
            decided_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn queue_conflict_resolution_uses_normal_task_path_and_dedup_key() {
        match plan_integration_conflict(&decision(FeedbackOutcomeKind::QueueConflictResolution)) {
            ConflictResolutionPlan::QueueResolutionTask {
                task_id,
                dedup_key,
                rationale,
            } => {
                assert_eq!(task_id, "T-ic");
                assert_eq!(dedup_key, conflict_dedup_key("T-ic"));
                assert!(!rationale.is_empty());
            }
            other => panic!("expected QueueResolutionTask, got {other:?}"),
        }
    }

    #[test]
    fn request_repair_keeps_destructive_actions_operator_approved() {
        // RequestRepair never carries `force=true`; the plan only routes
        // through the existing integration repair path.
        match plan_integration_conflict(&decision(FeedbackOutcomeKind::RequestIntegrationRepair)) {
            ConflictResolutionPlan::RequestRepair {
                task_id, dedup_key, ..
            } => {
                assert_eq!(task_id, "T-ic");
                assert_eq!(dedup_key, conflict_dedup_key("T-ic"));
            }
            other => panic!("expected RequestRepair, got {other:?}"),
        }
    }

    #[test]
    fn integration_remains_blocked_via_consistent_dedup_key() {
        // Two calls produce the same dedup key — downstream callers
        // dedupe on this so the integration path stays blocked until
        // the original conflict-resolution task resolves.
        let first =
            plan_integration_conflict(&decision(FeedbackOutcomeKind::QueueConflictResolution));
        let second =
            plan_integration_conflict(&decision(FeedbackOutcomeKind::QueueConflictResolution));
        let first_key = match &first {
            ConflictResolutionPlan::QueueResolutionTask { dedup_key, .. } => dedup_key.clone(),
            _ => panic!("expected QueueResolutionTask"),
        };
        let second_key = match &second {
            ConflictResolutionPlan::QueueResolutionTask { dedup_key, .. } => dedup_key.clone(),
            _ => panic!("expected QueueResolutionTask"),
        };
        assert_eq!(first_key, second_key);
    }

    #[test]
    fn queue_without_task_id_escalates_instead_of_empty_dedup_key() {
        let mut decision = decision(FeedbackOutcomeKind::QueueConflictResolution);
        decision.outcome.task_id = None;
        decision.brief.trigger.task_id = None;

        match plan_integration_conflict(&decision) {
            ConflictResolutionPlan::Escalate {
                cause: ConflictEscalateCause::MissingRequiredField,
                rationale,
                ..
            } => assert!(rationale.contains("queue_conflict_resolution requires task_id")),
            other => panic!("expected missing-field escalation, got {other:?}"),
        }
    }

    #[test]
    fn escalate_outcome_routes_to_escalation_with_supervisor_cause() {
        match plan_integration_conflict(&decision(FeedbackOutcomeKind::Escalate)) {
            ConflictResolutionPlan::Escalate {
                cause: ConflictEscalateCause::SupervisorChoice,
                ..
            } => {}
            other => panic!("expected supervisor escalation, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_outcomes_escalate_with_unsupported_cause() {
        match plan_integration_conflict(&decision(FeedbackOutcomeKind::PromoteReviewerFollowup)) {
            ConflictResolutionPlan::Escalate {
                cause: ConflictEscalateCause::UnsupportedOutcome,
                ..
            } => {}
            other => panic!("expected unsupported escalation, got {other:?}"),
        }
    }

    #[test]
    fn wrong_trigger_kind_escalates_with_mismatch_cause() {
        let mut bad = decision(FeedbackOutcomeKind::QueueConflictResolution);
        bad.brief.trigger.kind = FeedbackTriggerKind::WorkerFailed;
        match plan_integration_conflict(&bad) {
            ConflictResolutionPlan::Escalate {
                cause: ConflictEscalateCause::TriggerKindMismatch,
                ..
            } => {}
            other => panic!("expected mismatch escalation, got {other:?}"),
        }
    }
}
