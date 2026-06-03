//! Feedback decision application planner.
//!
//! Pure functions that translate a `ValidatedOutcome` into an `ApplyPlan`
//! describing what concrete Brehon action should run. Keeping this layer
//! pure means the supervisor crate never has to depend on brehon-mcp or
//! brehon-orchestrator; downstream wiring in those crates consumes the
//! plan, calls the appropriate MCP/orchestrator path, and emits
//! `FeedbackApplied` / `FeedbackFailed` events.
//!
//! All apply paths must use existing Brehon mechanisms:
//!
//! - **PromoteFollowup** → `task action=promote_followups followup_ids=…`
//! - **WaiveFollowup**   → `task action=waive_followups followup_ids=… reason=…`
//! - **FoldIntoRework** / **RequestRework** → release worker through the
//!   existing review/rework path; the task moves back to in_progress.
//! - **RetryRun**        → schedule a retry via the orchestrator retry
//!   policy; honors max attempts/backoff.
//! - **QueueConflictResolution** / **RequestIntegrationRepair** → create
//!   a conflict-resolution task via the normal task action path.
//! - **EnterDrain** → emit `SystemDraining` event; the supervisor honors it
//!   on next tick.
//! - **EnterSafeMode** → route through an explicit runtime executor; without
//!   one, lifecycle records `FeedbackFailed`.
//! - **NudgeWorker**     → send through existing NudgeSender.
//! - **RequestOperatorClarification** / **Escalate** → record an
//!   `EscalationTriggered` event so the operator sees the request.
//! - **NoAction**        → record no downstream effect.

use brehon_types::{FeedbackDecision, FeedbackOutcome, FeedbackOutcomeKind, FeedbackTriggerKind};

/// A plan describing exactly which Brehon MCP / orchestrator action the
/// downstream applier should invoke for a validated feedback decision.
/// Each variant carries the minimal fields needed to invoke an existing
/// Brehon path. Downstream callers translate these into the actual tool
/// arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyPlan {
    /// No downstream effect: record the decision and stop.
    NoAction { rationale: String },
    /// Send a nudge to the worker session.
    NudgeWorker {
        task_id: Option<String>,
        run_id: Option<String>,
        rationale: String,
    },
    /// Promote a reviewer follow-up via `task action=promote_followups`.
    PromoteFollowup {
        task_id: String,
        followup_id: String,
        rationale: String,
    },
    /// Waive a reviewer follow-up via `task action=waive_followups`.
    WaiveFollowup {
        task_id: String,
        followup_id: String,
        rationale: String,
    },
    /// Request rework on the worker side; the task goes back into
    /// `changes_requested` and the worker picks it up.
    RequestRework { task_id: String, rationale: String },
    /// Retry a failed run through the orchestrator retry policy.
    RetryRun {
        task_id: Option<String>,
        run_id: Option<String>,
        rationale: String,
    },
    /// Queue a conflict-resolution task for an integration conflict.
    QueueConflictResolution { task_id: String, rationale: String },
    /// Request a non-destructive integration repair path.
    RequestIntegrationRepair { task_id: String, rationale: String },
    /// Ask the operator for clarification by emitting an escalation event.
    RequestOperatorClarification {
        task_id: Option<String>,
        rationale: String,
    },
    /// Enter system drain mode.
    EnterDrain { rationale: String },
    /// Enter safe mode through an explicit runtime executor.
    EnterSafeMode { rationale: String },
    /// Escalate to a human operator.
    Escalate {
        task_id: Option<String>,
        rationale: String,
    },
}

impl ApplyPlan {
    /// Stable label for telemetry and the decision summary.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NoAction { .. } => "no_action",
            Self::NudgeWorker { .. } => "nudge_worker",
            Self::PromoteFollowup { .. } => "promote_followup",
            Self::WaiveFollowup { .. } => "waive_followup",
            Self::RequestRework { .. } => "request_rework",
            Self::RetryRun { .. } => "retry_run",
            Self::QueueConflictResolution { .. } => "queue_conflict_resolution",
            Self::RequestIntegrationRepair { .. } => "request_integration_repair",
            Self::RequestOperatorClarification { .. } => "request_operator_clarification",
            Self::EnterDrain { .. } => "enter_drain",
            Self::EnterSafeMode { .. } => "enter_safe_mode",
            Self::Escalate { .. } => "escalate",
        }
    }
}

/// Plan an apply action for a validated feedback decision.
///
/// `validate_outcome` rejects trigger/outcome mismatches before this point,
/// but this planner still fails closed for callers that accidentally pass an
/// unvalidated decision.
pub fn plan_application(decision: &FeedbackDecision) -> ApplyPlan {
    let outcome = &decision.outcome;
    let trigger_kind = decision.brief.trigger.kind;
    let trigger_task_id = decision
        .brief
        .trigger
        .task_id
        .as_ref()
        .map(|id| id.as_str().to_string());
    let trigger_run_id = decision
        .brief
        .trigger
        .run_id
        .as_ref()
        .map(|id| id.as_str().to_string());
    let outcome_task = outcome
        .task_id
        .as_ref()
        .map(|id| id.as_str().to_string())
        .or_else(|| trigger_task_id.clone());
    let outcome_run = outcome
        .run_id
        .as_ref()
        .map(|id| id.as_str().to_string())
        .or_else(|| trigger_run_id.clone());

    match outcome.kind {
        FeedbackOutcomeKind::NoAction => ApplyPlan::NoAction {
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::NudgeWorker => ApplyPlan::NudgeWorker {
            task_id: outcome_task,
            run_id: outcome_run,
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::PromoteReviewerFollowup => {
            if !matches!(trigger_kind, FeedbackTriggerKind::ReviewerFollowup) {
                return ApplyPlan::Escalate {
                    task_id: outcome_task,
                    rationale: format!(
                        "Cannot promote reviewer follow-up from trigger kind {}.",
                        trigger_kind.as_str()
                    ),
                };
            }
            let Some(task_id) = outcome_task else {
                return missing_field_escalation(None, "task_id", outcome);
            };
            let Some(followup_id) = non_empty_string(outcome.followup_id.as_deref()) else {
                return missing_field_escalation(Some(task_id), "followup_id", outcome);
            };
            ApplyPlan::PromoteFollowup {
                task_id,
                followup_id,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::WaiveReviewerFollowup => {
            if !matches!(trigger_kind, FeedbackTriggerKind::ReviewerFollowup) {
                return ApplyPlan::Escalate {
                    task_id: outcome_task,
                    rationale: format!(
                        "Cannot waive reviewer follow-up from trigger kind {}.",
                        trigger_kind.as_str()
                    ),
                };
            }
            let Some(task_id) = outcome_task else {
                return missing_field_escalation(None, "task_id", outcome);
            };
            let Some(followup_id) = non_empty_string(outcome.followup_id.as_deref()) else {
                return missing_field_escalation(Some(task_id), "followup_id", outcome);
            };
            ApplyPlan::WaiveFollowup {
                task_id,
                followup_id,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::RequestRework => {
            let Some(task_id) = outcome_task else {
                return missing_field_escalation(None, "task_id", outcome);
            };
            ApplyPlan::RequestRework {
                task_id,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::RetryRun => ApplyPlan::RetryRun {
            task_id: outcome_task,
            run_id: outcome_run,
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::QueueConflictResolution => {
            let Some(task_id) = outcome_task else {
                return missing_field_escalation(None, "task_id", outcome);
            };
            ApplyPlan::QueueConflictResolution {
                task_id,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::RequestIntegrationRepair => {
            let Some(task_id) = outcome_task else {
                return missing_field_escalation(None, "task_id", outcome);
            };
            ApplyPlan::RequestIntegrationRepair {
                task_id,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::RequestOperatorClarification => {
            ApplyPlan::RequestOperatorClarification {
                task_id: outcome_task,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::EnterDrain => ApplyPlan::EnterDrain {
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::EnterSafeMode => ApplyPlan::EnterSafeMode {
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::Escalate => ApplyPlan::Escalate {
            task_id: outcome_task,
            rationale: outcome.rationale.clone(),
        },
    }
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn missing_field_escalation(
    task_id: Option<String>,
    field: &str,
    outcome: &FeedbackOutcome,
) -> ApplyPlan {
    ApplyPlan::Escalate {
        task_id,
        rationale: format!(
            "Validated outcome {} could not be applied because {field} was missing.",
            outcome.kind.as_str()
        ),
    }
}

/// Helper: is the outcome plan a fold-into-rework path? Used by tests
/// and downstream wiring that wants to treat rework as the canonical
/// "fold follow-up into the next worker round" action.
pub fn is_fold_into_rework(outcome: &FeedbackOutcome) -> bool {
    matches!(outcome.kind, FeedbackOutcomeKind::RequestRework)
}

/// Helper: is the trigger a reviewer follow-up that this outcome targets?
pub fn outcome_targets_reviewer_followup(decision: &FeedbackDecision) -> bool {
    matches!(
        decision.brief.trigger.kind,
        FeedbackTriggerKind::ReviewerFollowup
    )
}

#[cfg(test)]
mod reviewer_followup_tests {
    use super::*;
    use brehon_types::{
        EventId, FeedbackBrief, FeedbackBriefSection, FeedbackOutcome, FeedbackOutcomeKind,
        FeedbackTrigger, FeedbackTriggerId, FeedbackTriggerKind, FeedbackTurnId, ReviewId, TaskId,
        FEEDBACK_CONTRACT_VERSION,
    };
    use std::collections::BTreeSet;

    fn brief_for_followup() -> FeedbackBrief {
        let trigger = FeedbackTrigger {
            trigger_id: FeedbackTriggerId::new("fb-rev-1"),
            kind: FeedbackTriggerKind::ReviewerFollowup,
            task_id: Some(TaskId::new("T-rev")),
            run_id: None,
            review_id: Some(ReviewId::new("REV-rev")),
            source_event_ids: vec![EventId::new(11)],
            covered_event_range: Some((EventId::new(11), EventId::new(11))),
            summary: "Open follow-up FUP-1".into(),
            payload: serde_json::json!({"followup_id": "FUP-1"}),
            created_at: chrono::Utc::now(),
        };
        let mut allowed: BTreeSet<FeedbackOutcomeKind> = BTreeSet::new();
        for kind in FeedbackOutcomeKind::all() {
            allowed.insert(*kind);
        }
        FeedbackBrief {
            turn_id: FeedbackTurnId::new("turn-rev"),
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

    fn decision_for(kind: FeedbackOutcomeKind, followup_id: Option<&str>) -> FeedbackDecision {
        let brief = brief_for_followup();
        let outcome = FeedbackOutcome {
            contract_version: FEEDBACK_CONTRACT_VERSION,
            turn_id: brief.turn_id.clone(),
            trigger_id: brief.trigger.trigger_id.clone(),
            kind,
            rationale: "supervisor decided".into(),
            followup_id: followup_id.map(|id| id.to_string()),
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
    fn promote_followup_plan_carries_followup_and_task_ids() {
        let decision = decision_for(FeedbackOutcomeKind::PromoteReviewerFollowup, Some("FUP-1"));
        match plan_application(&decision) {
            ApplyPlan::PromoteFollowup {
                task_id,
                followup_id,
                rationale,
            } => {
                assert_eq!(task_id, "T-rev");
                assert_eq!(followup_id, "FUP-1");
                assert!(!rationale.is_empty());
            }
            other => panic!("expected PromoteFollowup, got {other:?}"),
        }
        assert!(outcome_targets_reviewer_followup(&decision));
    }

    #[test]
    fn waive_followup_plan_carries_required_fields() {
        let decision = decision_for(FeedbackOutcomeKind::WaiveReviewerFollowup, Some("FUP-1"));
        match plan_application(&decision) {
            ApplyPlan::WaiveFollowup {
                task_id,
                followup_id,
                ..
            } => {
                assert_eq!(task_id, "T-rev");
                assert_eq!(followup_id, "FUP-1");
            }
            other => panic!("expected WaiveFollowup, got {other:?}"),
        }
    }

    #[test]
    fn request_rework_is_fold_into_rework_path() {
        let decision = decision_for(FeedbackOutcomeKind::RequestRework, None);
        assert!(is_fold_into_rework(&decision.outcome));
        match plan_application(&decision) {
            ApplyPlan::RequestRework { task_id, .. } => assert_eq!(task_id, "T-rev"),
            other => panic!("expected RequestRework, got {other:?}"),
        }
    }

    #[test]
    fn request_operator_clarification_records_task_when_known() {
        let decision = decision_for(FeedbackOutcomeKind::RequestOperatorClarification, None);
        match plan_application(&decision) {
            ApplyPlan::RequestOperatorClarification { task_id, .. } => {
                assert_eq!(task_id.as_deref(), Some("T-rev"))
            }
            other => panic!("expected RequestOperatorClarification, got {other:?}"),
        }
    }

    #[test]
    fn escalate_records_task_for_review_followup_trigger() {
        let decision = decision_for(FeedbackOutcomeKind::Escalate, None);
        match plan_application(&decision) {
            ApplyPlan::Escalate { task_id, .. } => assert_eq!(task_id.as_deref(), Some("T-rev")),
            other => panic!("expected Escalate, got {other:?}"),
        }
    }
}
