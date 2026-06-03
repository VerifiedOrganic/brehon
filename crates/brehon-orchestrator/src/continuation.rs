//! Pure continuation decision policy.
//!
//! This module does not send prompts or mutate run state. It classifies whether
//! an existing claimed run may receive a bounded same-run continuation prompt.

use brehon_types::{ContinuationPolicyConfig, RunStatus, TaskStatus};

use crate::task_lifecycle::TaskLifecycle;

pub type ContinuationPolicy = ContinuationPolicyConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationSessionHealth {
    Healthy,
    Unhealthy,
    Unknown,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationReviewState {
    NotInReview,
    WaitingForReview,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinuationInput {
    pub task_status: TaskStatus,
    pub run_status: RunStatus,
    pub session_health: ContinuationSessionHealth,
    pub turn_count: u32,
    pub run_matches_task: bool,
    pub session_matches_task: bool,
    pub review_state: Option<ContinuationReviewState>,
    pub idle_for_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationDecision {
    ContinueNow {
        next_turn: u32,
        reason: &'static str,
    },
    Stop {
        reason: &'static str,
    },
    Escalate {
        reason: &'static str,
    },
    NoAction {
        reason: &'static str,
    },
}

pub fn decide_continuation(
    input: ContinuationInput,
    policy: ContinuationPolicy,
) -> ContinuationDecision {
    if !policy.enabled {
        return ContinuationDecision::NoAction {
            reason: "continuation policy is disabled",
        };
    }

    if policy.max_turns_per_run == 0 || policy.idle_prompt_after_secs == 0 {
        return ContinuationDecision::Escalate {
            reason: "continuation policy is invalid",
        };
    }

    if TaskLifecycle::is_terminal(input.task_status) {
        return ContinuationDecision::Stop {
            reason: "task is terminal",
        };
    }

    if matches!(
        input.task_status,
        TaskStatus::InReview | TaskStatus::ChangesRequested | TaskStatus::Approved
    ) || matches!(
        input.review_state,
        Some(
            ContinuationReviewState::WaitingForReview
                | ContinuationReviewState::ChangesRequested
                | ContinuationReviewState::Approved
        )
    ) {
        return ContinuationDecision::Stop {
            reason: "task is in review state",
        };
    }

    if !matches!(
        input.task_status,
        TaskStatus::Assigned | TaskStatus::InProgress
    ) {
        return ContinuationDecision::NoAction {
            reason: "task is not active",
        };
    }

    match input.run_status {
        RunStatus::Claimed | RunStatus::Running => {}
        RunStatus::Completed | RunStatus::Failed | RunStatus::Abandoned => {
            return ContinuationDecision::Stop {
                reason: "run is terminal",
            };
        }
        RunStatus::RetryQueued => {
            return ContinuationDecision::NoAction {
                reason: "run is retry queued",
            };
        }
        RunStatus::Created | RunStatus::Released => {
            return ContinuationDecision::NoAction {
                reason: "run is not claimed",
            };
        }
    }

    match input.session_health {
        ContinuationSessionHealth::Healthy => {}
        ContinuationSessionHealth::Unhealthy | ContinuationSessionHealth::Missing => {
            return ContinuationDecision::Escalate {
                reason: "session is not healthy",
            };
        }
        ContinuationSessionHealth::Unknown => {
            return ContinuationDecision::NoAction {
                reason: "session health is unknown",
            };
        }
    }

    if !input.run_matches_task || !input.session_matches_task {
        return ContinuationDecision::Escalate {
            reason: "run ownership does not match active task",
        };
    }

    if input.turn_count >= policy.max_turns_per_run {
        return ContinuationDecision::Stop {
            reason: "continuation turn cap reached",
        };
    }

    if input.idle_for_secs < policy.idle_prompt_after_secs {
        return ContinuationDecision::NoAction {
            reason: "run has not been idle long enough",
        };
    }

    ContinuationDecision::ContinueNow {
        next_turn: input.turn_count.saturating_add(1),
        reason: "active run is eligible for continuation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ContinuationInput {
        ContinuationInput {
            task_status: TaskStatus::InProgress,
            run_status: RunStatus::Running,
            session_health: ContinuationSessionHealth::Healthy,
            turn_count: 0,
            run_matches_task: true,
            session_matches_task: true,
            review_state: Some(ContinuationReviewState::NotInReview),
            idle_for_secs: 300,
        }
    }

    #[test]
    fn continuation_active_run_continues_now() {
        assert_eq!(
            decide_continuation(input(), ContinuationPolicy::default()),
            ContinuationDecision::ContinueNow {
                next_turn: 1,
                reason: "active run is eligible for continuation",
            }
        );
    }

    #[test]
    fn continuation_terminal_task_stops() {
        let mut input = input();
        input.task_status = TaskStatus::Merged;

        assert_eq!(
            decide_continuation(input, ContinuationPolicy::default()),
            ContinuationDecision::Stop {
                reason: "task is terminal",
            }
        );
    }

    #[test]
    fn continuation_review_state_stops() {
        let mut input = input();
        input.task_status = TaskStatus::InReview;

        assert_eq!(
            decide_continuation(input, ContinuationPolicy::default()),
            ContinuationDecision::Stop {
                reason: "task is in review state",
            }
        );
    }

    #[test]
    fn continuation_unhealthy_session_escalates() {
        let mut input = input();
        input.session_health = ContinuationSessionHealth::Unhealthy;

        assert_eq!(
            decide_continuation(input, ContinuationPolicy::default()),
            ContinuationDecision::Escalate {
                reason: "session is not healthy",
            }
        );
    }

    #[test]
    fn continuation_max_turn_cap_stops() {
        let mut input = input();
        input.turn_count = ContinuationPolicy::default().max_turns_per_run;

        assert_eq!(
            decide_continuation(input, ContinuationPolicy::default()),
            ContinuationDecision::Stop {
                reason: "continuation turn cap reached",
            }
        );
    }
}
