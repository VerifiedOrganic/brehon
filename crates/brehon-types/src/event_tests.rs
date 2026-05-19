use super::*;
use crate::{
    ClaimGeneration, ClaimOwner, ProofBlocker, ProofBlockerStatus, ProofBundleId,
    ProofBundleStatus, ProofCheck, ProofCheckStatus, ProofCommand, ProofDecision, ProofIntegration,
    ProofReview, ReviewId, ReviewScore, ReviewVerdict, RunId, RunRole, RunStatus, SessionId,
    TaskId,
};
use chrono::{DateTime, TimeZone, Utc};

fn ts() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap()
}

fn run_id() -> RunId {
    RunId::new("run-1")
}

fn task_id() -> TaskId {
    TaskId::new("t")
}

fn proof_bundle_id() -> ProofBundleId {
    ProofBundleId::new("proof-t")
}

fn proof_command() -> ProofCommand {
    ProofCommand {
        run_id: Some(run_id()),
        command: "cargo test -p brehon-types event".into(),
        cwd: Some("/repo".into()),
        exit_code: Some(0),
        started_at: ts(),
        completed_at: Some(ts()),
        output_summary: Some("event tests passed".into()),
        evidence_ref: None,
    }
}

fn proof_check() -> ProofCheck {
    ProofCheck {
        name: "event tests".into(),
        command: Some("cargo test -p brehon-types event".into()),
        status: ProofCheckStatus::Passed,
        summary: Some("passed".into()),
        evidence_ref: None,
        checked_at: ts(),
    }
}

fn proof_review() -> ProofReview {
    ProofReview {
        review_id: ReviewId::new("review-1"),
        reviewer_id: Some("reviewer-1".into()),
        score: Some(ReviewScore::new(8)),
        verdict: Some(ReviewVerdict::Approve),
        findings: vec!["no blockers".into()],
        followups: Vec::new(),
        reviewed_at: ts(),
    }
}

fn proof_integration() -> ProofIntegration {
    ProofIntegration {
        status: "integrated".into(),
        branch: Some("task/t".into()),
        base_branch: Some("main".into()),
        worktree_path: Some("/repo/.worktrees/t".into()),
        commit: Some("abc1234".into()),
        summary: Some("merged cleanly".into()),
        conflicts: Vec::new(),
        integrated_at: ts(),
    }
}

fn proof_decision() -> ProofDecision {
    ProofDecision {
        decision_id: Some("decision-1".into()),
        decided_by: "supervisor".into(),
        decision: "accept proof event shape".into(),
        reason: Some("P5.2".into()),
        decided_at: ts(),
    }
}

fn proof_blocker() -> ProofBlocker {
    ProofBlocker {
        blocker_id: Some("blocker-1".into()),
        summary: "proof store not implemented yet".into(),
        source: Some("P5.2 scope".into()),
        status: ProofBlockerStatus::Open,
        created_at: ts(),
        resolved_at: None,
        resolution: None,
    }
}

#[test]
fn event_id_ordering() {
    let id1 = EventId::new(1);
    let id2 = EventId::new(2);
    assert!(id1 < id2);
}

#[test]
fn event_envelope_serialization() {
    let envelope = EventEnvelope {
        event: Event {
            kind: EventKind::TaskCreated {
                task_id: "T001".into(),
            },
            timestamp: ts(),
            aggregate_id: "T001".into(),
        },
        event_id: EventId::new(42),
        correlation_id: Some(CorrelationId::new("corr-1")),
        causation_id: None,
        idempotency_key: None,
    };
    let json = serde_json::to_string(&envelope).unwrap();
    let parsed: EventEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(envelope, parsed);
}

#[test]
fn event_envelope_versioned_wire_round_trip() {
    let envelope = EventEnvelope {
        event: Event {
            kind: EventKind::TaskCreated {
                task_id: "T001-v1".into(),
            },
            timestamp: ts(),
            aggregate_id: "T001-v1".into(),
        },
        event_id: EventId::new(1),
        correlation_id: None,
        causation_id: None,
        idempotency_key: None,
    };

    let serialized = serialize_event_envelope(&envelope).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&serialized).unwrap();

    assert_eq!(
        parsed["v"],
        serde_json::json!(VersionedEventEnvelope::CURRENT_VERSION)
    );
    assert!(parsed["envelope"].is_object());

    let decoded = deserialize_event_envelope(&serialized).unwrap();
    assert_eq!(envelope, decoded);
}

#[test]
fn event_envelope_legacy_backward_compat() {
    let legacy = r#"{
        "event": {
            "kind": {"TaskCreated": {"task_id": "T-legacy"}},
            "timestamp": "2024-01-01T00:00:00+00:00",
            "aggregate_id": "task-legacy"
        },
        "event_id": 123,
        "correlation_id": null,
        "causation_id": null,
        "idempotency_key": null
    }"#;

    let decoded = deserialize_event_envelope(legacy.as_bytes()).unwrap();
    assert_eq!(
        decoded,
        EventEnvelope {
            event: Event {
                kind: EventKind::TaskCreated {
                    task_id: "T-legacy".into(),
                },
                timestamp: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00")
                    .unwrap()
                    .with_timezone(&Utc),
                aggregate_id: "task-legacy".into(),
            },
            event_id: EventId::new(123),
            correlation_id: None,
            causation_id: None,
            idempotency_key: None,
        }
    );
}

#[test]
fn rejects_unsupported_future_version() {
    let json = r#"{
        "v": 2,
        "envelope": {
            "event": {
                "kind": {"TaskCreated": {"task_id": "T-future"}},
                "timestamp": "2024-01-04T00:00:00+00:00",
                "aggregate_id": "future-fixture"
            },
            "event_id": 101,
            "correlation_id": null,
            "causation_id": null,
            "idempotency_key": null
        }
    }"#;

    let result = deserialize_event_envelope(json.as_bytes());
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("unsupported"));
}

#[test]
fn rejects_legacy_shape_with_version_marker() {
    let json = r#"{
        "v": 2,
        "event": {
            "kind": {"TaskCreated": {"task_id": "T-legacy-marked"}},
            "timestamp": "2024-01-05T00:00:00+00:00",
            "aggregate_id": "legacy-marked-fixture"
        },
        "event_id": 102,
        "correlation_id": null,
        "causation_id": null,
        "idempotency_key": null
    }"#;

    let result = deserialize_event_envelope(json.as_bytes());
    assert!(result.is_err());
}

#[test]
fn event_filter_builder() {
    let filter = EventFilter::new().aggregate("T001").limit(100);
    assert_eq!(filter.aggregate_id, Some("T001".into()));
    assert_eq!(filter.limit, Some(100));
}

#[test]
fn proof_event_index_helpers_return_task_and_review_ids() {
    let task_event = EventKind::ProofBundleFinalized {
        proof_bundle_id: proof_bundle_id(),
        task_id: task_id(),
        final_status: ProofBundleStatus::Complete,
        finalized_at: ts(),
    };
    let review_event = EventKind::ProofReviewLinked {
        proof_bundle_id: proof_bundle_id(),
        task_id: task_id(),
        review: proof_review(),
        linked_at: ts(),
    };

    assert_eq!(task_event.task_id(), Some("t"));
    assert_eq!(review_event.review_id(), Some("review-1"));
}

#[test]
fn all_event_kinds_covered() {
    let kinds = vec![
        EventKind::AgentSpawned {
            agent_id: "a".into(),
            session_id: "s".into(),
            role: "r".into(),
        },
        EventKind::AgentDied {
            agent_id: "a".into(),
            session_id: "s".into(),
            reason: "r".into(),
        },
        EventKind::PromptSent {
            session_id: "s".into(),
            prompt_id: "p".into(),
            content: "c".into(),
        },
        EventKind::PromptCancelled {
            session_id: "s".into(),
            prompt_id: "p".into(),
            reason: "r".into(),
        },
        EventKind::ResponseReceived {
            session_id: "s".into(),
            prompt_id: "p".into(),
            tokens_used: 100,
        },
        EventKind::PermissionRequested {
            session_id: "s".into(),
            permission_id: "p".into(),
            action: "a".into(),
        },
        EventKind::PermissionResolved {
            session_id: "s".into(),
            permission_id: "p".into(),
            approved: true,
        },
        EventKind::OperationStarted {
            session_id: "s".into(),
            operation: "o".into(),
        },
        EventKind::OperationCompleted {
            session_id: "s".into(),
            operation: "o".into(),
            success: true,
        },
        EventKind::TaskCreated {
            task_id: "t".into(),
        },
        EventKind::TaskAssigned {
            task_id: "t".into(),
            agent_id: "a".into(),
        },
        EventKind::TaskCompleted {
            task_id: "t".into(),
        },
        EventKind::RunCreated {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            status: RunStatus::Created,
        },
        EventKind::RunClaimed {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            owner: ClaimOwner::new("worker-1"),
            session_id: Some(SessionId::new("s")),
            generation: ClaimGeneration::new(1),
            lease_expires_at: ts(),
        },
        EventKind::RunClaimRenewed {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            owner: ClaimOwner::new("worker-1"),
            generation: ClaimGeneration::new(1),
            lease_expires_at: ts(),
        },
        EventKind::RunStarted {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            generation: ClaimGeneration::new(1),
            started_at: ts(),
        },
        EventKind::RunActivityObserved {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            generation: ClaimGeneration::new(1),
            activity: "checkpoint".into(),
            observed_at: ts(),
        },
        EventKind::RunReleased {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            generation: ClaimGeneration::new(1),
            reason: Some("worker unavailable".into()),
            released_at: ts(),
        },
        EventKind::RunRetryQueued {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            generation: ClaimGeneration::new(1),
            reason: "retry requested".into(),
            queued_at: ts(),
            retry_at: Some(ts()),
        },
        EventKind::RunCompleted {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            generation: ClaimGeneration::new(1),
            completed_at: ts(),
        },
        EventKind::RunFailed {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            generation: ClaimGeneration::new(1),
            reason: "test failed".into(),
            failed_at: ts(),
        },
        EventKind::RunAbandoned {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            generation: ClaimGeneration::new(1),
            reason: "operator abandoned".into(),
            abandoned_at: ts(),
        },
        EventKind::StaleRunMutationRejected {
            run_id: run_id(),
            task_id: task_id(),
            role: RunRole::Worker,
            attempted_generation: ClaimGeneration::new(1),
            current_generation: ClaimGeneration::new(2),
            mutation: "complete_run".into(),
        },
        EventKind::ProofBundleCreated {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            run_ids: vec![run_id()],
            created_at: ts(),
        },
        EventKind::ProofCommandRecorded {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            command: proof_command(),
            recorded_at: ts(),
        },
        EventKind::ProofCheckRecorded {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            check: proof_check(),
            is_test_result: true,
            recorded_at: ts(),
        },
        EventKind::ProofReviewLinked {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            review: proof_review(),
            linked_at: ts(),
        },
        EventKind::ProofIntegrationRecorded {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            integration: proof_integration(),
            recorded_at: ts(),
        },
        EventKind::ProofDecisionRecorded {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            scope: ProofDecisionScope::Supervisor,
            decision: proof_decision(),
            recorded_at: ts(),
        },
        EventKind::ProofBlockerRecorded {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            blocker: proof_blocker(),
            recorded_at: ts(),
        },
        EventKind::ProofBundleFinalized {
            proof_bundle_id: proof_bundle_id(),
            task_id: task_id(),
            final_status: ProofBundleStatus::Complete,
            finalized_at: ts(),
        },
        EventKind::ReviewRequested {
            task_id: "t".into(),
            review_id: "r".into(),
        },
        EventKind::ReviewScoreReceived {
            review_id: "r".into(),
            reviewer_id: "rv".into(),
            score: 8,
        },
        EventKind::ReviewApproved {
            review_id: "r".into(),
        },
        EventKind::ReviewRejected {
            review_id: "r".into(),
        },
        EventKind::ReviewChangesRequested {
            review_id: "r".into(),
        },
        EventKind::MergePrepared {
            task_id: "t".into(),
            branch: "b".into(),
        },
        EventKind::MergeCommitted {
            task_id: "t".into(),
        },
        EventKind::MergeAborted {
            task_id: "t".into(),
            reason: "r".into(),
        },
        EventKind::EpicBranchCreated {
            epic_id: "e".into(),
            branch: "b".into(),
            base_commit: "c".into(),
        },
        EventKind::SubtaskBranchCreated {
            subtask_id: "s".into(),
            branch: "b".into(),
            base_branch: "epic/b".into(),
        },
        EventKind::SubtaskIntegrated {
            subtask_id: "s".into(),
            epic_id: "e".into(),
            branch: "b".into(),
        },
        EventKind::NudgeSent {
            session_id: "s".into(),
            kind: "soft".into(),
            content: "c".into(),
        },
        EventKind::NudgeAcknowledged {
            session_id: "s".into(),
            nudge_kind: "soft".into(),
        },
        EventKind::NudgeActedOn {
            session_id: "s".into(),
            nudge_kind: "soft".into(),
            progress_type: "percent".into(),
        },
        EventKind::NudgeTimedOut {
            session_id: "s".into(),
            nudge_id: "n".into(),
            nudge_kind: "soft".into(),
            elapsed_secs: 120,
        },
        EventKind::MemoryCreated {
            memory_id: "m".into(),
            content: "c".into(),
            tags: vec!["tag".into()],
            source_agent: Some("agent".into()),
        },
        EventKind::MemoryDeleted {
            memory_id: "m".into(),
        },
        EventKind::StuckDetected {
            session_id: "s".into(),
            duration_minutes: 10,
            pattern: None,
        },
        EventKind::EscalationTriggered {
            reason: "r".into(),
            context: "c".into(),
        },
        EventKind::SystemDraining { reason: "r".into() },
        EventKind::WorkerReassigned {
            old_worker: "worker-1".into(),
            new_worker: "worker-2".into(),
            task_id: "T001".into(),
            reason: "stalled".into(),
            worktree_state: "clean".into(),
        },
        EventKind::FeedbackTriggerDetected {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            dedup_key: "reviewer_followup|T001|-|r|followup:FUP-1".into(),
            kind: crate::feedback::FeedbackTriggerKind::ReviewerFollowup,
            task_id: Some(task_id()),
            run_id: None,
            review_id: Some("r".into()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            summary: "review followup".into(),
            detected_at: ts(),
        },
        EventKind::FeedbackBriefBuilt {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            run_id: None,
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            contract_version: crate::feedback::FEEDBACK_CONTRACT_VERSION,
            total_bytes: 256,
            section_count: 4,
            truncated: false,
            has_missing_context: false,
            built_at: ts(),
        },
        EventKind::FeedbackTurnStarted {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            supervisor_id: Some("supervisor-1".into()),
            started_at: ts(),
        },
        EventKind::FeedbackOutcomeReceived {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            outcome_kind: crate::feedback::FeedbackOutcomeKind::PromoteReviewerFollowup,
            received_at: ts(),
        },
        EventKind::FeedbackOutcomeValidated {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            outcome_kind: crate::feedback::FeedbackOutcomeKind::PromoteReviewerFollowup,
            validated_at: ts(),
        },
        EventKind::FeedbackOutcomeRejected {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            outcome_kind: Some(crate::feedback::FeedbackOutcomeKind::EnterDrain),
            reason: crate::feedback::FeedbackOutcomeRejectionReason::OutcomeKindDenied,
            message: "drain mode not enabled".into(),
            rejected_at: ts(),
        },
        EventKind::FeedbackDecisionRecorded {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            outcome_kind: crate::feedback::FeedbackOutcomeKind::PromoteReviewerFollowup,
            decided_at: ts(),
            decision_summary: "promoted FUP-1 to T-2".into(),
        },
        EventKind::FeedbackApplied {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            outcome_kind: crate::feedback::FeedbackOutcomeKind::PromoteReviewerFollowup,
            application_summary: "created task T-2".into(),
            applied_at: ts(),
        },
        EventKind::FeedbackFailed {
            trigger_id: crate::feedback::FeedbackTriggerId::new("fb-1"),
            turn_id: crate::feedback::FeedbackTurnId::new("turn-1"),
            task_id: Some(task_id()),
            source_event_ids: vec![EventId::new(7)],
            covered_event_range: Some((EventId::new(7), EventId::new(7))),
            outcome_kind: crate::feedback::FeedbackOutcomeKind::PromoteReviewerFollowup,
            error: "downstream MCP call failed".into(),
            failed_at: ts(),
        },
    ];

    for kind in kinds {
        let json = serde_json::to_string(&kind).unwrap();
        let parsed: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, parsed);
    }
}
