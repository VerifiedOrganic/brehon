//! Durable feedback lifecycle recording.
//!
//! This module is the evented boundary around the pure Phase 6 pieces. It
//! persists detected triggers, records brief/outcome/decision lifecycle events,
//! invokes an explicit application executor, and writes the compact TUI cache.

use async_trait::async_trait;
use chrono::Utc;

use brehon_ports::{EventStore, PortError};
use brehon_types::{
    Event, EventKind, FeedbackBrief, FeedbackClarificationSummary, FeedbackDecisionSummary,
    FeedbackEscalationSummary, FeedbackOutcome, FeedbackOutcomeKind, FeedbackPolicy,
    FeedbackTaskSummary, FeedbackTrigger, FeedbackTriggerSummary, TaskId,
};

use super::apply::{plan_application, ApplyPlan};
use super::cache::write_feedback_cache;
use super::outcome::{validate_outcome, OutcomeValidation};

/// Applies a validated feedback plan through Brehon's existing authorities.
///
/// Implementations live at composition boundaries that already own MCP,
/// orchestrator, runtime-command, or gateway access. This crate never reaches
/// around those authorities directly.
#[async_trait]
pub trait FeedbackActionExecutor: Send + Sync {
    async fn apply_feedback_plan(&self, plan: &ApplyPlan) -> Result<String, String>;
}

/// Executor for tests and deployments that only want event-native effects.
///
/// Task/run/prompt mutations fail closed with `FeedbackFailed`; they do not
/// get reported as applied without an explicit executor.
#[derive(Debug, Default)]
pub struct EventOnlyFeedbackActionExecutor;

#[async_trait]
impl FeedbackActionExecutor for EventOnlyFeedbackActionExecutor {
    async fn apply_feedback_plan(&self, plan: &ApplyPlan) -> Result<String, String> {
        match plan {
            ApplyPlan::NoAction { rationale } => Ok(format!("no action recorded: {rationale}")),
            ApplyPlan::RequestOperatorClarification { rationale, .. } => {
                Ok(format!("operator clarification requested: {rationale}"))
            }
            ApplyPlan::Escalate { rationale, .. } => Ok(format!("escalated: {rationale}")),
            ApplyPlan::EnterDrain { rationale } => Ok(format!("drain requested: {rationale}")),
            other => Err(format!(
                "no feedback action executor configured for {}",
                other.label()
            )),
        }
    }
}

/// Result of recording and applying one feedback outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackLifecycleResult {
    Rejected {
        reason: brehon_types::FeedbackOutcomeRejectionReason,
        message: String,
    },
    Applied {
        summary: String,
    },
    Failed {
        error: String,
    },
}

/// Persist newly-detected feedback triggers.
pub async fn record_detected_triggers(
    event_store: &dyn EventStore,
    triggers: &[FeedbackTrigger],
) -> Result<Vec<brehon_types::EventId>, PortError> {
    let mut ids = Vec::with_capacity(triggers.len());
    for trigger in triggers {
        let event = feedback_event(
            EventKind::FeedbackTriggerDetected {
                trigger_id: trigger.trigger_id.clone(),
                dedup_key: trigger.dedup_key(),
                kind: trigger.kind,
                task_id: trigger.task_id.clone(),
                run_id: trigger.run_id.clone(),
                review_id: trigger.review_id.as_ref().map(|id| id.as_str().to_string()),
                source_event_ids: trigger.source_event_ids.clone(),
                covered_event_range: trigger.covered_event_range,
                summary: trigger.summary.clone(),
                detected_at: Utc::now(),
            },
            trigger.task_id.as_ref(),
            trigger.trigger_id.as_str(),
        );
        ids.push(event_store.append(event).await?);
    }
    Ok(ids)
}

/// Persist that a bounded brief was built.
pub async fn record_brief_built(
    event_store: &dyn EventStore,
    brief: &FeedbackBrief,
) -> Result<brehon_types::EventId, PortError> {
    event_store
        .append(feedback_event(
            EventKind::FeedbackBriefBuilt {
                trigger_id: brief.trigger.trigger_id.clone(),
                turn_id: brief.turn_id.clone(),
                task_id: brief.trigger.task_id.clone(),
                run_id: brief.trigger.run_id.clone(),
                source_event_ids: brief.trigger.source_event_ids.clone(),
                covered_event_range: brief.trigger.covered_event_range,
                contract_version: brief.contract_version,
                total_bytes: brief.total_bytes,
                section_count: brief.sections.len(),
                truncated: brief.truncated,
                has_missing_context: brief.has_missing_context,
                built_at: Utc::now(),
            },
            brief.trigger.task_id.as_ref(),
            brief.trigger.trigger_id.as_str(),
        ))
        .await
}

/// Persist that a supervisor turn has started for a built feedback brief.
pub async fn record_turn_started(
    event_store: &dyn EventStore,
    brief: &FeedbackBrief,
    supervisor_id: Option<String>,
) -> Result<brehon_types::EventId, PortError> {
    event_store
        .append(feedback_event(
            EventKind::FeedbackTurnStarted {
                trigger_id: brief.trigger.trigger_id.clone(),
                turn_id: brief.turn_id.clone(),
                task_id: brief.trigger.task_id.clone(),
                source_event_ids: brief.trigger.source_event_ids.clone(),
                covered_event_range: brief.trigger.covered_event_range,
                supervisor_id,
                started_at: Utc::now(),
            },
            brief.trigger.task_id.as_ref(),
            brief.trigger.trigger_id.as_str(),
        ))
        .await
}

/// Record, validate, apply, and cache one supervisor feedback outcome.
pub async fn record_validate_and_apply_outcome(
    event_store: &dyn EventStore,
    brief: &FeedbackBrief,
    outcome: &FeedbackOutcome,
    policy: &FeedbackPolicy,
    executor: &dyn FeedbackActionExecutor,
) -> Result<FeedbackLifecycleResult, PortError> {
    record_turn_started(event_store, brief, outcome.supervisor_id.clone()).await?;
    append_outcome_received(event_store, brief, outcome).await?;

    match validate_outcome(brief, outcome, policy) {
        OutcomeValidation::Rejected { reason, message } => {
            event_store
                .append(feedback_event(
                    EventKind::FeedbackOutcomeRejected {
                        trigger_id: brief.trigger.trigger_id.clone(),
                        turn_id: brief.turn_id.clone(),
                        task_id: brief.trigger.task_id.clone(),
                        source_event_ids: brief.trigger.source_event_ids.clone(),
                        covered_event_range: brief.trigger.covered_event_range,
                        outcome_kind: Some(outcome.kind),
                        reason,
                        message: message.clone(),
                        rejected_at: Utc::now(),
                    },
                    brief.trigger.task_id.as_ref(),
                    brief.trigger.trigger_id.as_str(),
                ))
                .await?;
            Ok(FeedbackLifecycleResult::Rejected { reason, message })
        }
        OutcomeValidation::Accepted(validated) => {
            event_store
                .append(feedback_event(
                    EventKind::FeedbackOutcomeValidated {
                        trigger_id: brief.trigger.trigger_id.clone(),
                        turn_id: brief.turn_id.clone(),
                        task_id: brief.trigger.task_id.clone(),
                        source_event_ids: brief.trigger.source_event_ids.clone(),
                        covered_event_range: brief.trigger.covered_event_range,
                        outcome_kind: outcome.kind,
                        validated_at: Utc::now(),
                    },
                    brief.trigger.task_id.as_ref(),
                    brief.trigger.trigger_id.as_str(),
                ))
                .await?;

            let decision_summary = decision_summary(outcome);
            event_store
                .append(feedback_event(
                    EventKind::FeedbackDecisionRecorded {
                        trigger_id: brief.trigger.trigger_id.clone(),
                        turn_id: brief.turn_id.clone(),
                        task_id: brief.trigger.task_id.clone(),
                        source_event_ids: brief.trigger.source_event_ids.clone(),
                        covered_event_range: brief.trigger.covered_event_range,
                        outcome_kind: outcome.kind,
                        decided_at: validated.decision.decided_at,
                        decision_summary: decision_summary.clone(),
                    },
                    brief.trigger.task_id.as_ref(),
                    brief.trigger.trigger_id.as_str(),
                ))
                .await?;

            let plan = plan_application(&validated.decision);
            match executor.apply_feedback_plan(&plan).await {
                Ok(summary) => {
                    emit_event_native_effects(event_store, brief, outcome, &plan).await?;
                    event_store
                        .append(feedback_event(
                            EventKind::FeedbackApplied {
                                trigger_id: brief.trigger.trigger_id.clone(),
                                turn_id: brief.turn_id.clone(),
                                task_id: brief.trigger.task_id.clone(),
                                source_event_ids: brief.trigger.source_event_ids.clone(),
                                covered_event_range: brief.trigger.covered_event_range,
                                outcome_kind: outcome.kind,
                                application_summary: summary.clone(),
                                applied_at: Utc::now(),
                            },
                            brief.trigger.task_id.as_ref(),
                            brief.trigger.trigger_id.as_str(),
                        ))
                        .await?;
                    write_summary_cache(brief, outcome, &decision_summary);
                    Ok(FeedbackLifecycleResult::Applied { summary })
                }
                Err(error) => {
                    event_store
                        .append(feedback_event(
                            EventKind::FeedbackFailed {
                                trigger_id: brief.trigger.trigger_id.clone(),
                                turn_id: brief.turn_id.clone(),
                                task_id: brief.trigger.task_id.clone(),
                                source_event_ids: brief.trigger.source_event_ids.clone(),
                                covered_event_range: brief.trigger.covered_event_range,
                                outcome_kind: outcome.kind,
                                error: error.clone(),
                                failed_at: Utc::now(),
                            },
                            brief.trigger.task_id.as_ref(),
                            brief.trigger.trigger_id.as_str(),
                        ))
                        .await?;
                    Ok(FeedbackLifecycleResult::Failed { error })
                }
            }
        }
    }
}

async fn append_outcome_received(
    event_store: &dyn EventStore,
    brief: &FeedbackBrief,
    outcome: &FeedbackOutcome,
) -> Result<(), PortError> {
    event_store
        .append(feedback_event(
            EventKind::FeedbackOutcomeReceived {
                trigger_id: brief.trigger.trigger_id.clone(),
                turn_id: brief.turn_id.clone(),
                task_id: brief.trigger.task_id.clone(),
                source_event_ids: brief.trigger.source_event_ids.clone(),
                covered_event_range: brief.trigger.covered_event_range,
                outcome_kind: outcome.kind,
                received_at: Utc::now(),
            },
            brief.trigger.task_id.as_ref(),
            brief.trigger.trigger_id.as_str(),
        ))
        .await?;
    Ok(())
}

async fn emit_event_native_effects(
    event_store: &dyn EventStore,
    brief: &FeedbackBrief,
    outcome: &FeedbackOutcome,
    plan: &ApplyPlan,
) -> Result<(), PortError> {
    match plan {
        ApplyPlan::EnterDrain { rationale } => {
            event_store
                .append(feedback_event(
                    EventKind::SystemDraining {
                        reason: format!("feedback {}: {rationale}", brief.trigger.trigger_id),
                    },
                    brief.trigger.task_id.as_ref(),
                    brief.trigger.trigger_id.as_str(),
                ))
                .await?;
        }
        ApplyPlan::RequestOperatorClarification { rationale, .. }
        | ApplyPlan::Escalate { rationale, .. } => {
            event_store
                .append(feedback_event(
                    EventKind::EscalationTriggered {
                        reason: outcome.kind.as_str().to_string(),
                        context: format!("feedback {}: {rationale}", brief.trigger.trigger_id),
                    },
                    brief.trigger.task_id.as_ref(),
                    brief.trigger.trigger_id.as_str(),
                ))
                .await?;
        }
        _ => {}
    }
    Ok(())
}

fn write_summary_cache(brief: &FeedbackBrief, outcome: &FeedbackOutcome, decision: &str) {
    let Some(task_id) = brief.trigger.task_id.as_ref() else {
        return;
    };
    let mut summary = FeedbackTaskSummary {
        active_triggers: vec![FeedbackTriggerSummary {
            trigger_id: brief.trigger.trigger_id.as_str().to_string(),
            kind: brief.trigger.kind.as_str().to_string(),
            summary: brief.trigger.summary.clone(),
            created_at: brief.trigger.created_at.to_rfc3339(),
        }],
        recent_decisions: vec![FeedbackDecisionSummary {
            trigger_id: brief.trigger.trigger_id.as_str().to_string(),
            turn_id: brief.turn_id.as_str().to_string(),
            outcome_kind: outcome.kind.as_str().to_string(),
            summary: decision.to_string(),
            decided_at: Utc::now().to_rfc3339(),
        }],
        updated_at: Some(Utc::now().to_rfc3339()),
        ..FeedbackTaskSummary::default()
    };
    match outcome.kind {
        FeedbackOutcomeKind::RequestOperatorClarification => {
            summary
                .pending_clarifications
                .push(FeedbackClarificationSummary {
                    trigger_id: brief.trigger.trigger_id.as_str().to_string(),
                    question: outcome.rationale.clone(),
                    requested_at: Utc::now().to_rfc3339(),
                });
        }
        FeedbackOutcomeKind::Escalate => {
            summary.escalations.push(FeedbackEscalationSummary {
                trigger_id: brief.trigger.trigger_id.as_str().to_string(),
                reason: outcome.rationale.clone(),
                raised_at: Utc::now().to_rfc3339(),
            });
        }
        FeedbackOutcomeKind::EnterDrain => summary.drain_active = true,
        FeedbackOutcomeKind::EnterSafeMode => summary.safe_mode_active = true,
        _ => {}
    }
    write_feedback_cache(task_id.as_str(), &summary);
}

fn feedback_event(kind: EventKind, task_id: Option<&TaskId>, fallback: &str) -> Event {
    Event {
        kind,
        timestamp: Utc::now(),
        aggregate_id: task_id
            .map(|id| id.as_str().to_string())
            .unwrap_or_else(|| fallback.to_string()),
    }
}

fn decision_summary(outcome: &FeedbackOutcome) -> String {
    let mut parts = vec![outcome.kind.as_str().to_string()];
    if let Some(task_id) = outcome.task_id.as_ref() {
        parts.push(format!("task={}", task_id.as_str()));
    }
    if let Some(run_id) = outcome.run_id.as_ref() {
        parts.push(format!("run={}", run_id.as_str()));
    }
    if let Some(followup_id) = outcome.followup_id.as_ref() {
        parts.push(format!("followup={followup_id}"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::InMemoryEventStore;
    use brehon_types::{
        EventId, FeedbackBriefSection, FeedbackOutcomeRejectionReason, FeedbackTriggerId,
        FeedbackTriggerKind, FeedbackTurnId, ReviewId, RunId, TaskId, FEEDBACK_CONTRACT_VERSION,
    };
    use std::collections::BTreeSet;

    struct AcceptingExecutor;

    #[async_trait]
    impl FeedbackActionExecutor for AcceptingExecutor {
        async fn apply_feedback_plan(&self, plan: &ApplyPlan) -> Result<String, String> {
            Ok(format!("applied {}", plan.label()))
        }
    }

    struct FailingExecutor;

    #[async_trait]
    impl FeedbackActionExecutor for FailingExecutor {
        async fn apply_feedback_plan(&self, plan: &ApplyPlan) -> Result<String, String> {
            Err(format!("failed {}", plan.label()))
        }
    }

    fn trigger() -> FeedbackTrigger {
        FeedbackTrigger {
            trigger_id: FeedbackTriggerId::new("fb-reviewer-T-1-FUP-1"),
            kind: FeedbackTriggerKind::ReviewerFollowup,
            task_id: Some(TaskId::new("T-1")),
            run_id: Some(RunId::new("run-1")),
            review_id: Some(ReviewId::new("REV-1")),
            source_event_ids: vec![EventId::new(10)],
            covered_event_range: Some((EventId::new(10), EventId::new(12))),
            summary: "Open follow-up FUP-1".into(),
            payload: serde_json::json!({"followup_id": "FUP-1"}),
            created_at: Utc::now(),
        }
    }

    fn brief() -> FeedbackBrief {
        let mut allowed = BTreeSet::new();
        allowed.insert(FeedbackOutcomeKind::PromoteReviewerFollowup);
        FeedbackBrief {
            turn_id: FeedbackTurnId::new("turn-1"),
            contract_version: FEEDBACK_CONTRACT_VERSION,
            trigger: trigger(),
            sections: vec![FeedbackBriefSection::present("trigger", "body")],
            total_bytes: 4,
            truncated: false,
            has_missing_context: false,
            allowed_outcomes: allowed,
            rationale_max_chars: 256,
            rationale_min_chars: 8,
            built_at: Utc::now(),
        }
    }

    fn outcome(brief: &FeedbackBrief) -> FeedbackOutcome {
        FeedbackOutcome {
            contract_version: FEEDBACK_CONTRACT_VERSION,
            turn_id: brief.turn_id.clone(),
            trigger_id: brief.trigger.trigger_id.clone(),
            kind: FeedbackOutcomeKind::PromoteReviewerFollowup,
            rationale: "promote the blocking follow-up".into(),
            followup_id: Some("FUP-1".into()),
            task_id: brief.trigger.task_id.clone(),
            run_id: None,
            supervisor_id: Some("supervisor-1".into()),
            payload: serde_json::json!({}),
        }
    }

    fn drain_brief_and_outcome() -> (FeedbackBrief, FeedbackOutcome, FeedbackPolicy) {
        let mut brief = brief();
        brief
            .allowed_outcomes
            .insert(FeedbackOutcomeKind::EnterDrain);
        let mut outcome = outcome(&brief);
        outcome.kind = FeedbackOutcomeKind::EnterDrain;
        outcome.followup_id = None;
        outcome.rationale = "enter drain until feedback is reviewed".into();
        let mut policy = FeedbackPolicy::conservative();
        policy
            .allowed_outcomes
            .insert(FeedbackOutcomeKind::EnterDrain);
        policy.allow_drain = true;
        (brief, outcome, policy)
    }

    #[tokio::test]
    async fn trigger_and_decision_lifecycle_events_are_durable() {
        let store = InMemoryEventStore::new();
        let trigger = trigger();
        record_detected_triggers(&store, std::slice::from_ref(&trigger))
            .await
            .unwrap();
        let brief = brief();
        record_brief_built(&store, &brief).await.unwrap();
        let result = record_validate_and_apply_outcome(
            &store,
            &brief,
            &outcome(&brief),
            &FeedbackPolicy::conservative(),
            &AcceptingExecutor,
        )
        .await
        .unwrap();

        assert!(matches!(result, FeedbackLifecycleResult::Applied { .. }));
        let events = store.stream(None, 20).await.unwrap();
        assert!(events.iter().any(|(event, _)| matches!(
            event.kind,
            EventKind::FeedbackTriggerDetected { ref dedup_key, .. }
                if dedup_key.contains("followup:FUP-1")
        )));
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::FeedbackTurnStarted { .. })));
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::FeedbackApplied { .. })));
    }

    #[tokio::test]
    async fn invalid_outcome_records_rejection_event() {
        let store = InMemoryEventStore::new();
        let brief = brief();
        let mut bad = outcome(&brief);
        bad.followup_id = None;
        let result = record_validate_and_apply_outcome(
            &store,
            &brief,
            &bad,
            &FeedbackPolicy::conservative(),
            &AcceptingExecutor,
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            FeedbackLifecycleResult::Rejected {
                reason: FeedbackOutcomeRejectionReason::MissingRequiredField,
                message: "Outcome kind promote_reviewer_followup requires a non-empty followup_id."
                    .into()
            }
        );
        let events = store.stream(None, 20).await.unwrap();
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::FeedbackOutcomeRejected { .. })));
    }

    #[tokio::test]
    async fn event_only_executor_fails_closed_for_mutating_task_action() {
        let store = InMemoryEventStore::new();
        let brief = brief();
        let result = record_validate_and_apply_outcome(
            &store,
            &brief,
            &outcome(&brief),
            &FeedbackPolicy::conservative(),
            &EventOnlyFeedbackActionExecutor,
        )
        .await
        .unwrap();

        assert!(matches!(result, FeedbackLifecycleResult::Failed { .. }));
        let events = store.stream(None, 20).await.unwrap();
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::FeedbackFailed { .. })));
    }

    #[tokio::test]
    async fn event_only_executor_fails_closed_for_safe_mode_without_runtime_executor() {
        let store = InMemoryEventStore::new();
        let mut brief = brief();
        brief
            .allowed_outcomes
            .insert(FeedbackOutcomeKind::EnterSafeMode);
        let mut outcome = outcome(&brief);
        outcome.kind = FeedbackOutcomeKind::EnterSafeMode;
        outcome.followup_id = None;
        outcome.rationale = "enter safe mode until an operator reviews feedback".into();
        let mut policy = FeedbackPolicy::conservative();
        policy
            .allowed_outcomes
            .insert(FeedbackOutcomeKind::EnterSafeMode);
        policy.allow_safe_mode = true;

        let result = record_validate_and_apply_outcome(
            &store,
            &brief,
            &outcome,
            &policy,
            &EventOnlyFeedbackActionExecutor,
        )
        .await
        .unwrap();

        assert!(matches!(result, FeedbackLifecycleResult::Failed { .. }));
        let events = store.stream(None, 20).await.unwrap();
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::FeedbackFailed { .. })));
    }

    #[tokio::test]
    async fn event_native_effects_emit_only_after_executor_accepts() {
        let (brief, outcome, policy) = drain_brief_and_outcome();
        let failing_store = InMemoryEventStore::new();
        let result = record_validate_and_apply_outcome(
            &failing_store,
            &brief,
            &outcome,
            &policy,
            &FailingExecutor,
        )
        .await
        .unwrap();

        assert!(matches!(result, FeedbackLifecycleResult::Failed { .. }));
        let events = failing_store.stream(None, 20).await.unwrap();
        assert!(!events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::SystemDraining { .. })));
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::FeedbackFailed { .. })));

        let applied_store = InMemoryEventStore::new();
        let result = record_validate_and_apply_outcome(
            &applied_store,
            &brief,
            &outcome,
            &policy,
            &EventOnlyFeedbackActionExecutor,
        )
        .await
        .unwrap();

        assert!(matches!(result, FeedbackLifecycleResult::Applied { .. }));
        let events = applied_store.stream(None, 20).await.unwrap();
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::SystemDraining { .. })));
        assert!(events
            .iter()
            .any(|(event, _)| matches!(event.kind, EventKind::FeedbackApplied { .. })));
    }
}
