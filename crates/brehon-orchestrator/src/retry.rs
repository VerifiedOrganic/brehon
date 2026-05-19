//! Pure retry decision policy.
//!
//! This module intentionally does not mutate run state. It classifies an
//! observed failure into a bounded decision that later phases can wire into
//! durable run transitions.

use brehon_types::{RetryPolicyConfig, RunStatus, TaskStatus};

use crate::task_lifecycle::TaskLifecycle;

pub type RetryPolicy = RetryPolicyConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryFailureKind {
    Transient,
    Interrupted,
    Deterministic,
    PermissionDenied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReviewStatus {
    NotInReview,
    WaitingForReview,
    Approved,
    ChangesRequested,
    NeedsOperator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPermissionState {
    NotRequired,
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOperatorOverride {
    ForceRetryNow,
    ForceRetryLater,
    ForceFailTerminal,
    SuppressRetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryInput {
    pub run_status: RunStatus,
    pub failure_kind: RetryFailureKind,
    pub attempt_count: u32,
    pub task_status: TaskStatus,
    pub review_status: Option<RetryReviewStatus>,
    pub permission_state: Option<RetryPermissionState>,
    pub operator_override: Option<RetryOperatorOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDecision {
    RetryNow {
        next_attempt: u32,
        reason: &'static str,
    },
    RetryLater {
        next_attempt: u32,
        delay_ms: u64,
        jitter_ms: u64,
        reason: &'static str,
    },
    FailTerminal {
        reason: &'static str,
    },
    Escalate {
        reason: &'static str,
    },
    NoAction {
        reason: &'static str,
    },
}

pub fn decide_retry(input: RetryInput, policy: RetryPolicy) -> RetryDecision {
    if TaskLifecycle::is_terminal(input.task_status) {
        return RetryDecision::NoAction {
            reason: "task is terminal",
        };
    }

    match input.run_status {
        RunStatus::Completed | RunStatus::Abandoned => {
            return RetryDecision::NoAction {
                reason: "run is terminal and not retryable",
            };
        }
        RunStatus::RetryQueued => {
            return RetryDecision::NoAction {
                reason: "run is already queued for retry",
            };
        }
        RunStatus::Created | RunStatus::Released => {
            return RetryDecision::Escalate {
                reason: "failure observed for unclaimed run",
            };
        }
        RunStatus::Claimed | RunStatus::Running | RunStatus::Failed => {}
    }

    if let Some(override_decision) = input.operator_override {
        return decide_operator_override(input, policy, override_decision);
    }

    if !policy.enabled {
        return RetryDecision::NoAction {
            reason: "retry policy is disabled",
        };
    }

    if policy.max_attempts == 0 || input.attempt_count == 0 {
        return RetryDecision::Escalate {
            reason: "retry policy or attempt count is invalid",
        };
    }

    match input.permission_state {
        Some(RetryPermissionState::Denied) => {
            return RetryDecision::Escalate {
                reason: "permission denied",
            };
        }
        Some(RetryPermissionState::Pending) => {
            return RetryDecision::NoAction {
                reason: "permission decision is pending",
            };
        }
        Some(RetryPermissionState::Approved | RetryPermissionState::NotRequired) | None => {}
    }

    if matches!(input.review_status, Some(RetryReviewStatus::NeedsOperator)) {
        return RetryDecision::Escalate {
            reason: "review state requires operator attention",
        };
    }

    if input.attempt_count >= policy.max_attempts {
        return RetryDecision::Escalate {
            reason: "retry attempts exhausted",
        };
    }

    let next_attempt = input.attempt_count.saturating_add(1);
    match input.failure_kind {
        RetryFailureKind::Transient => RetryDecision::RetryLater {
            next_attempt,
            delay_ms: retry_delay_ms(input.attempt_count, policy),
            jitter_ms: policy.jitter_ms,
            reason: "transient failure",
        },
        RetryFailureKind::Interrupted => RetryDecision::RetryNow {
            next_attempt,
            reason: "interrupted run",
        },
        RetryFailureKind::Deterministic => RetryDecision::FailTerminal {
            reason: "deterministic failure",
        },
        RetryFailureKind::PermissionDenied => RetryDecision::Escalate {
            reason: "permission denied",
        },
        RetryFailureKind::Unknown => RetryDecision::Escalate {
            reason: "unknown failure kind",
        },
    }
}

fn decide_operator_override(
    input: RetryInput,
    policy: RetryPolicy,
    override_decision: RetryOperatorOverride,
) -> RetryDecision {
    match override_decision {
        RetryOperatorOverride::ForceRetryNow => RetryDecision::RetryNow {
            next_attempt: input.attempt_count.saturating_add(1),
            reason: "operator forced retry now",
        },
        RetryOperatorOverride::ForceRetryLater => RetryDecision::RetryLater {
            next_attempt: input.attempt_count.saturating_add(1),
            delay_ms: retry_delay_ms(input.attempt_count.max(1), policy),
            jitter_ms: policy.jitter_ms,
            reason: "operator forced retry later",
        },
        RetryOperatorOverride::ForceFailTerminal => RetryDecision::FailTerminal {
            reason: "operator forced terminal failure",
        },
        RetryOperatorOverride::SuppressRetry => RetryDecision::NoAction {
            reason: "operator suppressed retry",
        },
    }
}

fn retry_delay_ms(attempt_count: u32, policy: RetryPolicy) -> u64 {
    let exponent = attempt_count.saturating_sub(1).min(31);
    let multiplier = 1_u64 << exponent;
    policy
        .base_delay_ms
        .saturating_mul(multiplier)
        .min(policy.max_delay_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(failure_kind: RetryFailureKind) -> RetryInput {
        RetryInput {
            run_status: RunStatus::Failed,
            failure_kind,
            attempt_count: 1,
            task_status: TaskStatus::InProgress,
            review_status: None,
            permission_state: Some(RetryPermissionState::NotRequired),
            operator_override: None,
        }
    }

    #[test]
    fn retry_transient_failure_retries_later() {
        let decision = decide_retry(input(RetryFailureKind::Transient), RetryPolicy::default());

        assert_eq!(
            decision,
            RetryDecision::RetryLater {
                next_attempt: 2,
                delay_ms: 10_000,
                jitter_ms: 250,
                reason: "transient failure",
            }
        );
    }

    #[test]
    fn retry_interrupted_failure_retries_now() {
        let decision = decide_retry(input(RetryFailureKind::Interrupted), RetryPolicy::default());

        assert_eq!(
            decision,
            RetryDecision::RetryNow {
                next_attempt: 2,
                reason: "interrupted run",
            }
        );
    }

    #[test]
    fn retry_deterministic_failure_fails_terminal() {
        let decision = decide_retry(
            input(RetryFailureKind::Deterministic),
            RetryPolicy::default(),
        );

        assert_eq!(
            decision,
            RetryDecision::FailTerminal {
                reason: "deterministic failure",
            }
        );
    }

    #[test]
    fn retry_permission_denied_escalates() {
        let mut retry_input = input(RetryFailureKind::Transient);
        retry_input.permission_state = Some(RetryPermissionState::Denied);

        let decision = decide_retry(retry_input, RetryPolicy::default());

        assert_eq!(
            decision,
            RetryDecision::Escalate {
                reason: "permission denied",
            }
        );
    }

    #[test]
    fn retry_max_attempts_exhausted_escalates() {
        let mut retry_input = input(RetryFailureKind::Transient);
        retry_input.attempt_count = 2;

        let decision = decide_retry(retry_input, RetryPolicy::default());

        assert_eq!(
            decision,
            RetryDecision::Escalate {
                reason: "retry attempts exhausted",
            }
        );
    }

    #[test]
    fn retry_terminal_task_no_action() {
        let mut retry_input = input(RetryFailureKind::Transient);
        retry_input.task_status = TaskStatus::Merged;

        let decision = decide_retry(retry_input, RetryPolicy::default());

        assert_eq!(
            decision,
            RetryDecision::NoAction {
                reason: "task is terminal",
            }
        );
    }

    #[test]
    fn retry_operator_override_forces_retry_now() {
        let mut retry_input = input(RetryFailureKind::Deterministic);
        retry_input.operator_override = Some(RetryOperatorOverride::ForceRetryNow);

        let decision = decide_retry(retry_input, RetryPolicy::default());

        assert_eq!(
            decision,
            RetryDecision::RetryNow {
                next_attempt: 2,
                reason: "operator forced retry now",
            }
        );
    }
}
