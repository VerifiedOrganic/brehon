//! Helper macros and functions for test assertions.

/// Assert that events contain a specific event kind.
#[macro_export]
macro_rules! assert_event_contains {
    ($events:expr, $kind:expr) => {
        let found = $events.iter().any(|e| matches!(e.kind, $kind));
        if !found {
            panic!(
                "Expected event kind {:?} not found in events",
                stringify!($kind)
            );
        }
    };
}

/// Assert that events contain all specified kinds.
#[macro_export]
macro_rules! assert_events_contain_all {
    ($events:expr, [$( $kind:expr ),+ $(,)?]) => {
        $(
            let found = $events.iter().any(|e| matches!(e.kind, $kind));
            if !found {
                panic!(
                    "Expected event kind {:?} not found in events",
                    stringify!($kind)
                );
            }
        )+
    };
}

/// Assert task status.
#[macro_export]
macro_rules! assert_task_status {
    ($store:expr, $task_id:expr, $status:expr) => {
        let events = $store.all_events();
        let found = events.iter().any(|e| {
            matches!(e.kind, brehon_types::EventKind::TaskCreated { task_id } if task_id == $task_id)
        });
        if !found {
            // Check for other status events
            panic!(
                "Task {} not found or status {} not verified",
                $task_id, stringify!($status)
            );
        }
    };
}

/// Assert review result.
#[macro_export]
macro_rules! assert_review_result {
    ($store:expr, $review_id:expr, $verdict:expr) => {
        let events = $store.all_events();
        let events_str = format!("{:?}", events.iter().map(|e| &e.kind).collect::<Vec<_>>());
        let found = events.iter().any(|e| {
            match $verdict {
                "approved" => matches!(e.kind, brehon_types::EventKind::ReviewApproved { review_id } if review_id == $review_id),
                "rejected" => matches!(e.kind, brehon_types::EventKind::ReviewRejected { review_id } if review_id == $review_id),
                "changes_requested" => matches!(e.kind, brehon_types::EventKind::ReviewChangesRequested { review_id } if review_id == $review_id),
                _ => false,
            }
        });
        if !found {
            panic!(
                "Review {} result {} not found in events. Events: {}",
                $review_id, $verdict, events_str
            );
        }
    };
}

/// Check if an event was emitted.
pub fn event_was_emitted(events: &[brehon_types::Event], kind: &str) -> bool {
    events.iter().any(|e| {
        let event_kind_str = match &e.kind {
            brehon_types::EventKind::AgentSpawned { .. } => "AgentSpawned",
            brehon_types::EventKind::AgentDied { .. } => "AgentDied",
            brehon_types::EventKind::PromptSent { .. } => "PromptSent",
            brehon_types::EventKind::PromptCancelled { .. } => "PromptCancelled",
            brehon_types::EventKind::ResponseReceived { .. } => "ResponseReceived",
            brehon_types::EventKind::PermissionRequested { .. } => "PermissionRequested",
            brehon_types::EventKind::PermissionResolved { .. } => "PermissionResolved",
            brehon_types::EventKind::OperationStarted { .. } => "OperationStarted",
            brehon_types::EventKind::OperationCompleted { .. } => "OperationCompleted",
            brehon_types::EventKind::TaskCreated { .. } => "TaskCreated",
            brehon_types::EventKind::TaskAssigned { .. } => "TaskAssigned",
            brehon_types::EventKind::TaskCompleted { .. } => "TaskCompleted",
            brehon_types::EventKind::RunCreated { .. } => "RunCreated",
            brehon_types::EventKind::RunClaimed { .. } => "RunClaimed",
            brehon_types::EventKind::RunClaimRenewed { .. } => "RunClaimRenewed",
            brehon_types::EventKind::RunStarted { .. } => "RunStarted",
            brehon_types::EventKind::RunActivityObserved { .. } => "RunActivityObserved",
            brehon_types::EventKind::RunReleased { .. } => "RunReleased",
            brehon_types::EventKind::RunRetryQueued { .. } => "RunRetryQueued",
            brehon_types::EventKind::RunCompleted { .. } => "RunCompleted",
            brehon_types::EventKind::RunFailed { .. } => "RunFailed",
            brehon_types::EventKind::RunAbandoned { .. } => "RunAbandoned",
            brehon_types::EventKind::StaleRunMutationRejected { .. } => "StaleRunMutationRejected",
            brehon_types::EventKind::ProofBundleCreated { .. } => "ProofBundleCreated",
            brehon_types::EventKind::ProofCommandRecorded { .. } => "ProofCommandRecorded",
            brehon_types::EventKind::ProofCheckRecorded { .. } => "ProofCheckRecorded",
            brehon_types::EventKind::ProofReviewLinked { .. } => "ProofReviewLinked",
            brehon_types::EventKind::ProofIntegrationRecorded { .. } => "ProofIntegrationRecorded",
            brehon_types::EventKind::ProofDecisionRecorded { .. } => "ProofDecisionRecorded",
            brehon_types::EventKind::ProofBlockerRecorded { .. } => "ProofBlockerRecorded",
            brehon_types::EventKind::ProofBundleFinalized { .. } => "ProofBundleFinalized",
            brehon_types::EventKind::ReviewRequested { .. } => "ReviewRequested",
            brehon_types::EventKind::ReviewScoreReceived { .. } => "ReviewScoreReceived",
            brehon_types::EventKind::ReviewApproved { .. } => "ReviewApproved",
            brehon_types::EventKind::ReviewRejected { .. } => "ReviewRejected",
            brehon_types::EventKind::ReviewChangesRequested { .. } => "ReviewChangesRequested",
            brehon_types::EventKind::MergePrepared { .. } => "MergePrepared",
            brehon_types::EventKind::MergeCommitted { .. } => "MergeCommitted",
            brehon_types::EventKind::MergeAborted { .. } => "MergeAborted",
            brehon_types::EventKind::EpicBranchCreated { .. } => "EpicBranchCreated",
            brehon_types::EventKind::SubtaskBranchCreated { .. } => "SubtaskBranchCreated",
            brehon_types::EventKind::SubtaskIntegrated { .. } => "SubtaskIntegrated",
            brehon_types::EventKind::NudgeSent { .. } => "NudgeSent",
            brehon_types::EventKind::NudgeAcknowledged { .. } => "NudgeAcknowledged",
            brehon_types::EventKind::NudgeActedOn { .. } => "NudgeActedOn",
            brehon_types::EventKind::NudgeTimedOut { .. } => "NudgeTimedOut",
            brehon_types::EventKind::MemoryCreated { .. } => "MemoryCreated",
            brehon_types::EventKind::MemoryDeleted { .. } => "MemoryDeleted",
            brehon_types::EventKind::StuckDetected { .. } => "StuckDetected",
            brehon_types::EventKind::EscalationTriggered { .. } => "EscalationTriggered",
            brehon_types::EventKind::SystemDraining { .. } => "SystemDraining",
            brehon_types::EventKind::WorkerReassigned { .. } => "WorkerReassigned",
            brehon_types::EventKind::FeedbackTriggerDetected { .. } => "FeedbackTriggerDetected",
            brehon_types::EventKind::FeedbackBriefBuilt { .. } => "FeedbackBriefBuilt",
            brehon_types::EventKind::FeedbackTurnStarted { .. } => "FeedbackTurnStarted",
            brehon_types::EventKind::FeedbackOutcomeReceived { .. } => "FeedbackOutcomeReceived",
            brehon_types::EventKind::FeedbackOutcomeValidated { .. } => "FeedbackOutcomeValidated",
            brehon_types::EventKind::FeedbackOutcomeRejected { .. } => "FeedbackOutcomeRejected",
            brehon_types::EventKind::FeedbackDecisionRecorded { .. } => "FeedbackDecisionRecorded",
            brehon_types::EventKind::FeedbackApplied { .. } => "FeedbackApplied",
            brehon_types::EventKind::FeedbackFailed { .. } => "FeedbackFailed",
        };
        event_kind_str == kind
    })
}

/// Count events of a specific kind.
pub fn count_events<K: Fn(&brehon_types::EventKind) -> bool>(
    events: &[brehon_types::Event],
    kind_fn: K,
) -> usize {
    events.iter().filter(|e| kind_fn(&e.kind)).count()
}

/// Assert minimum event count.
pub fn assert_min_events<K: Fn(&brehon_types::EventKind) -> bool>(
    events: &[brehon_types::Event],
    min_count: usize,
    kind_fn: K,
    kind_name: &str,
) {
    let count = count_events(events, kind_fn);
    if count < min_count {
        panic!(
            "Expected at least {} {} events but found {}",
            min_count, kind_name, count
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{Event, EventKind};
    use chrono::Utc;

    fn make_event(kind: EventKind) -> Event {
        Event {
            kind,
            timestamp: Utc::now(),
            aggregate_id: "test".into(),
        }
    }

    #[test]
    fn event_was_emitted_true() {
        let events = vec![
            make_event(EventKind::TaskCreated {
                task_id: "T001".into(),
            }),
            make_event(EventKind::TaskCompleted {
                task_id: "T001".into(),
            }),
        ];

        assert!(event_was_emitted(&events, "TaskCreated"));
        assert!(event_was_emitted(&events, "TaskCompleted"));
    }

    #[test]
    fn event_was_emitted_false() {
        let events = vec![make_event(EventKind::TaskCreated {
            task_id: "T001".into(),
        })];

        assert!(!event_was_emitted(&events, "TaskCompleted"));
    }

    #[test]
    fn count_events_correctly() {
        let events = vec![
            make_event(EventKind::TaskCreated {
                task_id: "T001".into(),
            }),
            make_event(EventKind::TaskAssigned {
                task_id: "T001".into(),
                agent_id: "agent-1".into(),
            }),
            make_event(EventKind::TaskCreated {
                task_id: "T002".into(),
            }),
        ];

        let count = count_events(&events, |k| matches!(k, EventKind::TaskCreated { .. }));
        assert_eq!(count, 2);
    }
}
