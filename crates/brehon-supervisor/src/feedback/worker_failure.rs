//! Worker-failure decision helpers.
//!
//! Pure helpers that translate a feedback decision on a `worker_failed`
//! trigger into a `WorkerFailureRecovery` plan. The plan respects the
//! existing `RetryPolicyConfig` from `brehon-types` so the supervisor
//! cannot silently exceed the configured attempt cap.
//!
//! Downstream wiring (in brehon-orchestrator) consumes the plan and
//! invokes `decide_retry` / queues the retry on the durable run record.

use brehon_types::{FeedbackDecision, FeedbackOutcomeKind, RetryPolicyConfig};

/// Recovery action produced by [`plan_worker_failure_recovery`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerFailureRecovery {
    /// Schedule another retry attempt through the orchestrator retry policy.
    Retry {
        task_id: Option<String>,
        run_id: Option<String>,
        attempt_count: u32,
        max_attempts: u32,
        rationale: String,
    },
    /// Issue a nudge through the existing supervisor nudge path.
    Nudge {
        task_id: Option<String>,
        run_id: Option<String>,
        rationale: String,
    },
    /// Push the task back to the worker for rework.
    Rework { task_id: String, rationale: String },
    /// Visible escalation: retries exhausted or supervisor explicitly
    /// chose to escalate.
    Escalate {
        task_id: Option<String>,
        rationale: String,
        cause: EscalateCause,
    },
    /// Enter system drain.
    Drain { rationale: String },
    /// Enter safe mode.
    SafeMode { rationale: String },
    /// No-op (supervisor recorded no_action).
    NoAction,
}

/// Why escalation was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalateCause {
    /// Supervisor explicitly chose `Escalate`.
    SupervisorChoice,
    /// Retry attempt cap reached; no more retries are allowed.
    RetryExhausted,
    /// A mutation plan was missing the id needed to call the existing path.
    MissingRequiredField,
    /// Outcome kind is not valid for worker_failed triggers.
    UnsupportedOutcome,
}

/// Plan recovery for a worker failure trigger. `current_attempt` is the
/// attempt count of the failed run BEFORE incrementing for retry.
pub fn plan_worker_failure_recovery(
    decision: &FeedbackDecision,
    current_attempt: u32,
    retry_policy: &RetryPolicyConfig,
) -> WorkerFailureRecovery {
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
    let run_id = outcome
        .run_id
        .as_ref()
        .map(|id| id.as_str().to_string())
        .or_else(|| {
            decision
                .brief
                .trigger
                .run_id
                .as_ref()
                .map(|id| id.as_str().to_string())
        });

    match outcome.kind {
        FeedbackOutcomeKind::RetryRun => {
            if task_id.is_none() && run_id.is_none() {
                return WorkerFailureRecovery::Escalate {
                    task_id,
                    rationale: "Outcome kind retry_run requires either run_id or task_id."
                        .to_string(),
                    cause: EscalateCause::MissingRequiredField,
                };
            }
            if !retry_policy.enabled || current_attempt >= retry_policy.max_attempts {
                WorkerFailureRecovery::Escalate {
                    task_id,
                    rationale: format!(
                        "Retry exhausted: attempt {} of max {} (retry_enabled={}). {}",
                        current_attempt,
                        retry_policy.max_attempts,
                        retry_policy.enabled,
                        outcome.rationale
                    ),
                    cause: EscalateCause::RetryExhausted,
                }
            } else {
                WorkerFailureRecovery::Retry {
                    task_id,
                    run_id,
                    attempt_count: current_attempt,
                    max_attempts: retry_policy.max_attempts,
                    rationale: outcome.rationale.clone(),
                }
            }
        }
        FeedbackOutcomeKind::NudgeWorker => WorkerFailureRecovery::Nudge {
            task_id,
            run_id,
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::RequestRework => {
            let Some(task_id) = task_id else {
                return WorkerFailureRecovery::Escalate {
                    task_id: None,
                    rationale: "Outcome kind request_rework requires task_id.".to_string(),
                    cause: EscalateCause::MissingRequiredField,
                };
            };
            WorkerFailureRecovery::Rework {
                task_id,
                rationale: outcome.rationale.clone(),
            }
        }
        FeedbackOutcomeKind::Escalate => WorkerFailureRecovery::Escalate {
            task_id,
            rationale: outcome.rationale.clone(),
            cause: EscalateCause::SupervisorChoice,
        },
        FeedbackOutcomeKind::EnterDrain => WorkerFailureRecovery::Drain {
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::EnterSafeMode => WorkerFailureRecovery::SafeMode {
            rationale: outcome.rationale.clone(),
        },
        FeedbackOutcomeKind::NoAction => WorkerFailureRecovery::NoAction,
        _ => WorkerFailureRecovery::Escalate {
            task_id,
            rationale: format!(
                "Outcome kind {} is not supported on worker_failed triggers.",
                outcome.kind.as_str()
            ),
            cause: EscalateCause::UnsupportedOutcome,
        },
    }
}

#[cfg(test)]
mod worker_failure_tests {
    use super::*;
    use brehon_types::{
        EventId, FeedbackBrief, FeedbackBriefSection, FeedbackOutcome, FeedbackOutcomeKind,
        FeedbackTrigger, FeedbackTriggerId, FeedbackTriggerKind, FeedbackTurnId, RunId, TaskId,
        FEEDBACK_CONTRACT_VERSION,
    };
    use std::collections::BTreeSet;

    fn worker_failed_brief() -> FeedbackBrief {
        let trigger = FeedbackTrigger {
            trigger_id: FeedbackTriggerId::new("fb-wf-1"),
            kind: FeedbackTriggerKind::WorkerFailed,
            task_id: Some(TaskId::new("T-wf")),
            run_id: Some(RunId::new("run-1")),
            review_id: None,
            source_event_ids: vec![EventId::new(21)],
            covered_event_range: Some((EventId::new(21), EventId::new(21))),
            summary: "Run failed".into(),
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        let mut allowed: BTreeSet<FeedbackOutcomeKind> = BTreeSet::new();
        for kind in FeedbackOutcomeKind::all() {
            allowed.insert(*kind);
        }
        FeedbackBrief {
            turn_id: FeedbackTurnId::new("turn-wf"),
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
        let brief = worker_failed_brief();
        let outcome = FeedbackOutcome {
            contract_version: FEEDBACK_CONTRACT_VERSION,
            turn_id: brief.turn_id.clone(),
            trigger_id: brief.trigger.trigger_id.clone(),
            kind,
            rationale: "supervisor decision".into(),
            followup_id: None,
            task_id: brief.trigger.task_id.clone(),
            run_id: brief.trigger.run_id.clone(),
            supervisor_id: Some("supervisor-1".into()),
            payload: serde_json::json!({}),
        };
        FeedbackDecision {
            brief,
            outcome,
            decided_at: chrono::Utc::now(),
        }
    }

    fn policy(max_attempts: u32, enabled: bool) -> RetryPolicyConfig {
        let mut policy = RetryPolicyConfig::default();
        policy.enabled = enabled;
        policy.max_attempts = max_attempts;
        policy
    }

    #[test]
    fn retry_decision_within_budget_returns_retry_plan() {
        let result = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::RetryRun),
            0,
            &policy(3, true),
        );
        assert!(matches!(result, WorkerFailureRecovery::Retry { .. }));
    }

    #[test]
    fn retry_at_max_attempts_escalates_visibly() {
        let result = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::RetryRun),
            3,
            &policy(3, true),
        );
        match result {
            WorkerFailureRecovery::Escalate {
                cause, rationale, ..
            } => {
                assert_eq!(cause, EscalateCause::RetryExhausted);
                assert!(rationale.contains("Retry exhausted"));
            }
            other => panic!("expected escalation, got {other:?}"),
        }
    }

    #[test]
    fn retry_disabled_escalates_visibly() {
        let result = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::RetryRun),
            0,
            &policy(3, false),
        );
        match result {
            WorkerFailureRecovery::Escalate {
                cause, rationale, ..
            } => {
                assert_eq!(cause, EscalateCause::RetryExhausted);
                assert!(rationale.contains("retry_enabled=false"));
            }
            other => panic!("expected escalation, got {other:?}"),
        }
    }

    #[test]
    fn nudge_and_rework_and_escalate_map_to_their_recoveries() {
        let nudge = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::NudgeWorker),
            1,
            &policy(3, true),
        );
        assert!(matches!(nudge, WorkerFailureRecovery::Nudge { .. }));
        let rework = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::RequestRework),
            1,
            &policy(3, true),
        );
        match rework {
            WorkerFailureRecovery::Rework { task_id, .. } => assert_eq!(task_id, "T-wf"),
            other => panic!("expected rework, got {other:?}"),
        }
        let escalation = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::Escalate),
            1,
            &policy(3, true),
        );
        match escalation {
            WorkerFailureRecovery::Escalate {
                cause: EscalateCause::SupervisorChoice,
                ..
            } => {}
            other => panic!("expected supervisor escalation, got {other:?}"),
        }
    }

    #[test]
    fn request_rework_without_task_id_escalates_instead_of_empty_task() {
        let mut decision = decision(FeedbackOutcomeKind::RequestRework);
        decision.outcome.task_id = None;
        decision.brief.trigger.task_id = None;

        match plan_worker_failure_recovery(&decision, 1, &policy(3, true)) {
            WorkerFailureRecovery::Escalate {
                cause: EscalateCause::MissingRequiredField,
                rationale,
                ..
            } => assert!(rationale.contains("request_rework requires task_id")),
            other => panic!("expected missing-field escalation, got {other:?}"),
        }
    }

    #[test]
    fn drain_outcome_returns_drain_recovery() {
        let result = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::EnterDrain),
            0,
            &policy(3, true),
        );
        assert!(matches!(result, WorkerFailureRecovery::Drain { .. }));
    }

    #[test]
    fn unsupported_outcomes_escalate_with_unsupported_cause() {
        let result = plan_worker_failure_recovery(
            &decision(FeedbackOutcomeKind::PromoteReviewerFollowup),
            0,
            &policy(3, true),
        );
        match result {
            WorkerFailureRecovery::Escalate {
                cause: EscalateCause::UnsupportedOutcome,
                ..
            } => {}
            other => panic!("expected unsupported escalation, got {other:?}"),
        }
    }
}
