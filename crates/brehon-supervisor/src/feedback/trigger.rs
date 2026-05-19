//! Feedback trigger detector.
//!
//! `detect_triggers` is a pure function over `(events, snapshots, policy)`
//! that produces a deterministic, deduplicated set of `FeedbackTrigger`s.
//! Snapshots carry stateful context (open reviewer follow-ups, stale
//! claims, etc.) that the supervisor maintains elsewhere; the detector
//! treats them as inputs so it stays unit-testable.
//!
//! Triggers are deduplicated by `FeedbackTrigger::dedup_key`, so replaying
//! the same event range never produces duplicate triggers — see the
//! `dedup_triggers` helper which the public entry point applies.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};

use brehon_types::{
    Event, EventId, FeedbackTrigger, FeedbackTriggerId, FeedbackTriggerKind, ReviewId, RunId,
    TaskId,
};

/// Policy hooks for the trigger detector. These bounds control how the
/// detector classifies stuck-but-not-yet-timed-out workers and stale
/// claims that may need adjudication.
#[derive(Debug, Clone)]
pub struct TriggerDetectorPolicy {
    /// Treat a worker run as `worker_stuck` once a nudge has timed out
    /// and no follow-up activity was observed.
    pub stuck_on_nudge_timeout: bool,
    /// Treat retry-queued runs as `worker_failed` triggers when the run
    /// has run out of retries and reached escalation.
    pub failure_on_retry_exhausted: bool,
}

impl Default for TriggerDetectorPolicy {
    fn default() -> Self {
        Self {
            stuck_on_nudge_timeout: true,
            failure_on_retry_exhausted: true,
        }
    }
}

/// Snapshot of an open reviewer follow-up.
#[derive(Debug, Clone)]
pub struct ReviewerFollowupSnapshot {
    pub followup_id: String,
    pub task_id: String,
    pub review_id: Option<String>,
    pub severity: String,
    pub summary: String,
}

/// Snapshot of a worker run that has gone idle. The supervisor's stuck
/// detector produces these; the feedback detector promotes them to
/// `worker_stuck` triggers.
#[derive(Debug, Clone)]
pub struct RunActivitySnapshot {
    pub run_id: Option<RunId>,
    pub task_id: Option<TaskId>,
    pub session_id: Option<String>,
    pub stale_since: Option<DateTime<Utc>>,
    pub stale_reason: String,
}

/// Snapshot of a pending or denied permission request.
#[derive(Debug, Clone)]
pub struct PermissionSnapshot {
    pub permission_id: String,
    pub session_id: String,
    pub task_id: Option<TaskId>,
    pub status: String,
    pub action: String,
}

/// Snapshot of an active nudge whose timeout already fired.
#[derive(Debug, Clone)]
pub struct NudgeSnapshot {
    pub session_id: String,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub nudge_kind: String,
    pub elapsed_secs: u64,
}

/// Input to the trigger detector. All slices may be empty; the detector
/// is robust to missing snapshots and surfaces them as zero triggers
/// rather than panicking.
#[derive(Debug, Clone)]
pub struct FeedbackTriggerDetectorInput<'a> {
    /// Event range to scan (each entry is `(EventId, Event)`).
    pub events: &'a [(EventId, Event)],
    /// Open reviewer follow-ups from task state.
    pub open_followups: &'a [ReviewerFollowupSnapshot],
    /// Active stuck-run snapshots (from supervisor stuck detector).
    pub stuck_runs: &'a [RunActivitySnapshot],
    /// Pending permission requests.
    pub pending_permissions: &'a [PermissionSnapshot],
    /// Timed-out nudges.
    pub timed_out_nudges: &'a [NudgeSnapshot],
    /// Already-known trigger dedup keys from prior detector passes. The
    /// detector skips any newly-derived trigger whose dedup key collides
    /// with this set.
    pub known_dedup_keys: &'a HashSet<String>,
    /// Detector policy.
    pub policy: &'a TriggerDetectorPolicy,
}

/// Run the trigger detector. Returns the deterministically-ordered,
/// deduplicated trigger list.
pub fn detect_triggers(input: &FeedbackTriggerDetectorInput<'_>) -> Vec<FeedbackTrigger> {
    let mut triggers: Vec<FeedbackTrigger> = Vec::new();
    let covered = covered_range(input.events);

    // -- Event-driven triggers ---------------------------------------------
    for (event_id, event) in input.events {
        match &event.kind {
            brehon_types::EventKind::ReviewChangesRequested { review_id } => {
                triggers.push(make_trigger(
                    FeedbackTriggerKind::ReviewChangesRequested,
                    None,
                    None,
                    Some(ReviewId::new(review_id)),
                    vec![*event_id],
                    covered,
                    event.timestamp,
                    format!("Review {review_id} consolidated as changes_requested."),
                    serde_json::json!({ "review_id": review_id }),
                ));
            }
            brehon_types::EventKind::ReviewRejected { review_id } => {
                triggers.push(make_trigger(
                    FeedbackTriggerKind::ReviewChangesRequested,
                    None,
                    None,
                    Some(ReviewId::new(review_id)),
                    vec![*event_id],
                    covered,
                    event.timestamp,
                    format!("Review {review_id} consolidated as rejected."),
                    serde_json::json!({ "review_id": review_id }),
                ));
            }
            brehon_types::EventKind::RunFailed {
                run_id,
                task_id,
                generation,
                reason,
                failed_at,
                ..
            } => {
                let mut payload = serde_json::Map::new();
                payload.insert("generation".into(), serde_json::json!(generation.as_u64()));
                payload.insert("reason".into(), serde_json::json!(reason));
                triggers.push(make_trigger(
                    FeedbackTriggerKind::WorkerFailed,
                    Some(task_id.clone()),
                    Some(run_id.clone()),
                    None,
                    vec![*event_id],
                    covered,
                    *failed_at,
                    format!("Run {} failed: {reason}", run_id.as_str()),
                    serde_json::Value::Object(payload),
                ));
            }
            brehon_types::EventKind::RunAbandoned {
                run_id,
                task_id,
                reason,
                abandoned_at,
                ..
            } if input.policy.failure_on_retry_exhausted => {
                triggers.push(make_trigger(
                    FeedbackTriggerKind::WorkerFailed,
                    Some(task_id.clone()),
                    Some(run_id.clone()),
                    None,
                    vec![*event_id],
                    covered,
                    *abandoned_at,
                    format!("Run {} abandoned: {reason}", run_id.as_str()),
                    serde_json::json!({ "reason": reason }),
                ));
            }
            brehon_types::EventKind::StaleRunMutationRejected {
                run_id,
                task_id,
                attempted_generation,
                current_generation,
                mutation,
                ..
            } => {
                triggers.push(make_trigger(
                    FeedbackTriggerKind::StaleClaim,
                    Some(task_id.clone()),
                    Some(run_id.clone()),
                    None,
                    vec![*event_id],
                    covered,
                    event.timestamp,
                    format!(
                        "Stale run mutation '{mutation}' rejected for run {}: attempt gen {} vs current gen {}.",
                        run_id.as_str(),
                        attempted_generation.as_u64(),
                        current_generation.as_u64(),
                    ),
                    serde_json::json!({
                        "mutation": mutation,
                        "attempted_generation": attempted_generation.as_u64(),
                        "current_generation": current_generation.as_u64(),
                    }),
                ));
            }
            brehon_types::EventKind::ProofBlockerRecorded {
                task_id, blocker, ..
            } if matches!(blocker.status, brehon_types::ProofBlockerStatus::Open) => {
                triggers.push(make_trigger(
                    FeedbackTriggerKind::WorkerBlocked,
                    Some(task_id.clone()),
                    None,
                    None,
                    vec![*event_id],
                    covered,
                    event.timestamp,
                    format!(
                        "Open proof blocker on task {}: {}",
                        task_id.as_str(),
                        compact(&blocker.summary, 200)
                    ),
                    serde_json::json!({
                        "blocker_id": blocker.blocker_id,
                        "source": blocker.source,
                    }),
                ));
            }
            brehon_types::EventKind::ProofIntegrationRecorded {
                task_id,
                integration,
                ..
            } if !integration.conflicts.is_empty()
                || integration.status == "aborted"
                || integration.status == "conflict" =>
            {
                triggers.push(make_trigger(
                    FeedbackTriggerKind::IntegrationConflict,
                    Some(task_id.clone()),
                    None,
                    None,
                    vec![*event_id],
                    covered,
                    event.timestamp,
                    format!(
                        "Integration on task {} recorded status '{}' with {} conflict file(s).",
                        task_id.as_str(),
                        integration.status,
                        integration.conflicts.len(),
                    ),
                    serde_json::json!({
                        "status": integration.status,
                        "conflicts": integration.conflicts,
                        "branch": integration.branch,
                        "base_branch": integration.base_branch,
                    }),
                ));
            }
            brehon_types::EventKind::EscalationTriggered { reason, context } => {
                // Surface unrecognized escalations as close-gate blockers so
                // operators see them in the feedback lane. Existing
                // escalations remain authoritative; this is additive.
                triggers.push(make_trigger(
                    FeedbackTriggerKind::CloseGateBlocked,
                    task_id_from_aggregate(&event.aggregate_id),
                    None,
                    None,
                    vec![*event_id],
                    covered,
                    event.timestamp,
                    format!(
                        "Escalation triggered: {} — {}",
                        compact(reason, 80),
                        compact(context, 120),
                    ),
                    serde_json::json!({ "reason": reason }),
                ));
            }
            _ => {}
        }
    }

    // -- Snapshot-driven triggers ------------------------------------------

    for followup in input.open_followups {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "followup_id".into(),
            serde_json::Value::String(followup.followup_id.clone()),
        );
        payload.insert(
            "severity".into(),
            serde_json::Value::String(followup.severity.clone()),
        );
        triggers.push(make_trigger(
            FeedbackTriggerKind::ReviewerFollowup,
            Some(TaskId::new(&followup.task_id)),
            None,
            followup.review_id.as_deref().map(ReviewId::new),
            Vec::new(),
            covered,
            Utc::now(),
            format!(
                "Open reviewer follow-up {} on task {}: {}",
                followup.followup_id,
                followup.task_id,
                compact(&followup.summary, 160)
            ),
            serde_json::Value::Object(payload),
        ));
    }

    for stuck in input.stuck_runs {
        let summary = format!(
            "Run {} on task {} appears stuck: {}",
            stuck
                .run_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("(unknown)"),
            stuck
                .task_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("(unknown)"),
            compact(&stuck.stale_reason, 120),
        );
        let mut payload = serde_json::Map::new();
        if let Some(session) = &stuck.session_id {
            payload.insert(
                "session_id".into(),
                serde_json::Value::String(session.clone()),
            );
        }
        if let Some(since) = stuck.stale_since {
            payload.insert(
                "stale_since".into(),
                serde_json::Value::String(since.to_rfc3339()),
            );
        }
        triggers.push(make_trigger(
            FeedbackTriggerKind::WorkerStuck,
            stuck.task_id.clone(),
            stuck.run_id.clone(),
            None,
            Vec::new(),
            covered,
            stuck.stale_since.unwrap_or_else(Utc::now),
            summary,
            serde_json::Value::Object(payload),
        ));
    }

    if input.policy.stuck_on_nudge_timeout {
        for nudge in input.timed_out_nudges {
            triggers.push(make_trigger(
                FeedbackTriggerKind::WorkerStuck,
                nudge.task_id.clone(),
                nudge.run_id.clone(),
                None,
                Vec::new(),
                covered,
                Utc::now(),
                format!(
                    "Nudge '{}' timed out after {}s on session {}.",
                    nudge.nudge_kind, nudge.elapsed_secs, nudge.session_id,
                ),
                serde_json::json!({
                    "session_id": nudge.session_id,
                    "nudge_kind": nudge.nudge_kind,
                    "elapsed_secs": nudge.elapsed_secs,
                }),
            ));
        }
    }

    for permission in input.pending_permissions {
        triggers.push(make_trigger(
            FeedbackTriggerKind::PermissionBlocked,
            permission.task_id.clone(),
            None,
            None,
            Vec::new(),
            covered,
            Utc::now(),
            format!(
                "Permission '{}' status {} on session {} for action {}.",
                permission.permission_id,
                permission.status,
                permission.session_id,
                compact(&permission.action, 80),
            ),
            serde_json::json!({
                "permission_id": permission.permission_id,
                "status": permission.status,
                "session_id": permission.session_id,
            }),
        ));
    }

    dedup_triggers(triggers, input.known_dedup_keys)
}

/// Drop triggers whose dedup key collides with `known_dedup_keys` or with
/// an earlier trigger in the input list. Order is preserved for triggers
/// that survive; among new triggers, the first occurrence wins.
pub fn dedup_triggers(
    triggers: Vec<FeedbackTrigger>,
    known: &HashSet<String>,
) -> Vec<FeedbackTrigger> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<FeedbackTrigger> = Vec::with_capacity(triggers.len());
    let mut summaries: BTreeMap<String, String> = BTreeMap::new();
    for trigger in triggers {
        let key = trigger.dedup_key();
        if known.contains(&key) {
            continue;
        }
        if seen.insert(key.clone()) {
            summaries.insert(key, trigger.summary.clone());
            out.push(trigger);
        }
    }
    out
}

fn make_trigger(
    kind: FeedbackTriggerKind,
    task_id: Option<TaskId>,
    run_id: Option<RunId>,
    review_id: Option<ReviewId>,
    source_event_ids: Vec<EventId>,
    covered: Option<(EventId, EventId)>,
    created_at: DateTime<Utc>,
    summary: String,
    payload: serde_json::Value,
) -> FeedbackTrigger {
    let trigger_id = derive_trigger_id(
        kind,
        task_id.as_ref(),
        run_id.as_ref(),
        review_id.as_ref(),
        &payload,
    );
    FeedbackTrigger {
        trigger_id,
        kind,
        task_id,
        run_id,
        review_id,
        source_event_ids,
        covered_event_range: covered,
        summary,
        payload,
        created_at,
    }
}

fn derive_trigger_id(
    kind: FeedbackTriggerKind,
    task_id: Option<&TaskId>,
    run_id: Option<&RunId>,
    review_id: Option<&ReviewId>,
    payload: &serde_json::Value,
) -> FeedbackTriggerId {
    let task = task_id.map(|id| id.as_str()).unwrap_or("-");
    let run = run_id.map(|id| id.as_str()).unwrap_or("-");
    let review = review_id.map(|id| id.as_str()).unwrap_or("-");
    let scope = match kind {
        FeedbackTriggerKind::ReviewerFollowup => payload
            .get("followup_id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("-"),
        _ => "-",
    };
    FeedbackTriggerId::new(format!(
        "fb-{}-{}-{}-{}-{}",
        kind.as_str(),
        task,
        run,
        review,
        sanitize_id_component(scope)
    ))
}

fn sanitize_id_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn covered_range(events: &[(EventId, Event)]) -> Option<(EventId, EventId)> {
    let first = events.first().map(|(id, _)| *id)?;
    let last = events.last().map(|(id, _)| *id)?;
    if first <= last {
        Some((first, last))
    } else {
        Some((last, first))
    }
}

fn task_id_from_aggregate(aggregate_id: &str) -> Option<TaskId> {
    let trimmed = aggregate_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(TaskId::new(trimmed))
}

fn compact(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(max_chars + 1);
    for ch in trimmed.chars().take(max_chars.saturating_sub(1)) {
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod feedback_trigger_tests {
    use super::*;
    use brehon_types::{
        ClaimGeneration, Event, EventKind, ProofBlocker, ProofBlockerStatus, ProofBundleId,
        ProofIntegration, RunRole,
    };
    use chrono::TimeZone;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn event_at(seq: u64, kind: EventKind, aggregate: &str) -> (EventId, Event) {
        (
            EventId::new(seq),
            Event {
                kind,
                timestamp: ts(),
                aggregate_id: aggregate.to_string(),
            },
        )
    }

    static EMPTY_FOLLOWUPS: &[ReviewerFollowupSnapshot] = &[];
    static EMPTY_STUCK: &[RunActivitySnapshot] = &[];
    static EMPTY_PERMISSIONS: &[PermissionSnapshot] = &[];
    static EMPTY_NUDGES: &[NudgeSnapshot] = &[];
    static EMPTY_EVENTS: &[(EventId, Event)] = &[];

    fn base_input<'a>(
        known: &'a HashSet<String>,
        policy: &'a TriggerDetectorPolicy,
    ) -> FeedbackTriggerDetectorInput<'a> {
        FeedbackTriggerDetectorInput {
            events: EMPTY_EVENTS,
            open_followups: EMPTY_FOLLOWUPS,
            stuck_runs: EMPTY_STUCK,
            pending_permissions: EMPTY_PERMISSIONS,
            timed_out_nudges: EMPTY_NUDGES,
            known_dedup_keys: known,
            policy,
        }
    }

    #[test]
    fn detects_review_changes_requested_and_rejection_as_feedback() {
        let events = vec![
            event_at(
                1,
                EventKind::ReviewChangesRequested {
                    review_id: "REV-1".into(),
                },
                "REV-1",
            ),
            event_at(
                2,
                EventKind::ReviewRejected {
                    review_id: "REV-2".into(),
                },
                "REV-2",
            ),
        ];
        let known = HashSet::new();
        let policy = TriggerDetectorPolicy::default();
        let mut input = base_input(&known, &policy);
        input.events = &events;
        let triggers = detect_triggers(&input);
        assert_eq!(triggers.len(), 2);
        assert!(triggers
            .iter()
            .all(|t| matches!(t.kind, FeedbackTriggerKind::ReviewChangesRequested)));
    }

    #[test]
    fn detects_worker_failure_and_stale_claim() {
        let events = vec![
            event_at(
                3,
                EventKind::RunFailed {
                    run_id: RunId::new("run-1"),
                    task_id: TaskId::new("T-1"),
                    role: RunRole::Worker,
                    generation: ClaimGeneration::new(1),
                    reason: "cargo test failed".into(),
                    failed_at: ts(),
                },
                "T-1",
            ),
            event_at(
                4,
                EventKind::StaleRunMutationRejected {
                    run_id: RunId::new("run-1"),
                    task_id: TaskId::new("T-1"),
                    role: RunRole::Worker,
                    attempted_generation: ClaimGeneration::new(1),
                    current_generation: ClaimGeneration::new(2),
                    mutation: "complete_run".into(),
                },
                "T-1",
            ),
        ];
        let known = HashSet::new();
        let policy = TriggerDetectorPolicy::default();
        let mut input = base_input(&known, &policy);
        input.events = &events;
        let triggers = detect_triggers(&input);
        let kinds: Vec<FeedbackTriggerKind> = triggers.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&FeedbackTriggerKind::WorkerFailed));
        assert!(kinds.contains(&FeedbackTriggerKind::StaleClaim));
    }

    #[test]
    fn detects_open_blocker_as_worker_blocked_trigger() {
        let blocker = ProofBlocker {
            blocker_id: Some("B-1".into()),
            summary: "waiting for fixture".into(),
            source: None,
            status: ProofBlockerStatus::Open,
            created_at: ts(),
            resolved_at: None,
            resolution: None,
        };
        let events = vec![event_at(
            5,
            EventKind::ProofBlockerRecorded {
                proof_bundle_id: ProofBundleId::new("proof-T-1"),
                task_id: TaskId::new("T-1"),
                blocker,
                recorded_at: ts(),
            },
            "T-1",
        )];
        let known = HashSet::new();
        let policy = TriggerDetectorPolicy::default();
        let mut input = base_input(&known, &policy);
        input.events = &events;
        let triggers = detect_triggers(&input);
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].kind, FeedbackTriggerKind::WorkerBlocked);
    }

    #[test]
    fn detects_integration_conflict_from_proof_integration_event() {
        let integration = ProofIntegration {
            status: "conflict".into(),
            branch: Some("worker/T-1".into()),
            base_branch: Some("epic/x".into()),
            worktree_path: None,
            commit: None,
            summary: None,
            conflicts: vec!["src/lib.rs".into()],
            integrated_at: ts(),
        };
        let events = vec![event_at(
            6,
            EventKind::ProofIntegrationRecorded {
                proof_bundle_id: ProofBundleId::new("proof-T-1"),
                task_id: TaskId::new("T-1"),
                integration,
                recorded_at: ts(),
            },
            "T-1",
        )];
        let known = HashSet::new();
        let policy = TriggerDetectorPolicy::default();
        let mut input = base_input(&known, &policy);
        input.events = &events;
        let triggers = detect_triggers(&input);
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].kind, FeedbackTriggerKind::IntegrationConflict);
    }

    #[test]
    fn open_followups_and_stuck_runs_and_permissions_become_triggers() {
        let followups = vec![ReviewerFollowupSnapshot {
            followup_id: "FUP-1".into(),
            task_id: "T-1".into(),
            review_id: Some("REV-1".into()),
            severity: "blocking".into(),
            summary: "Add missing test".into(),
        }];
        let stuck = vec![RunActivitySnapshot {
            run_id: Some(RunId::new("run-1")),
            task_id: Some(TaskId::new("T-1")),
            session_id: Some("sess-1".into()),
            stale_since: Some(ts()),
            stale_reason: "no progress for 12 minutes".into(),
        }];
        let permissions = vec![PermissionSnapshot {
            permission_id: "perm-1".into(),
            session_id: "sess-1".into(),
            task_id: Some(TaskId::new("T-1")),
            status: "pending".into(),
            action: "write to /etc".into(),
        }];
        let nudges = vec![NudgeSnapshot {
            session_id: "sess-2".into(),
            task_id: Some(TaskId::new("T-2")),
            run_id: None,
            nudge_kind: "soft".into(),
            elapsed_secs: 480,
        }];
        let known = HashSet::new();
        let policy = TriggerDetectorPolicy::default();
        let mut input = base_input(&known, &policy);
        input.open_followups = &followups;
        input.stuck_runs = &stuck;
        input.pending_permissions = &permissions;
        input.timed_out_nudges = &nudges;
        let triggers = detect_triggers(&input);
        let kinds: Vec<FeedbackTriggerKind> = triggers.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&FeedbackTriggerKind::ReviewerFollowup));
        assert!(kinds.contains(&FeedbackTriggerKind::WorkerStuck));
        assert!(kinds.contains(&FeedbackTriggerKind::PermissionBlocked));
    }

    #[test]
    fn replay_does_not_create_duplicate_triggers() {
        let events = vec![event_at(
            1,
            EventKind::ReviewChangesRequested {
                review_id: "REV-1".into(),
            },
            "REV-1",
        )];
        let policy = TriggerDetectorPolicy::default();
        let known = HashSet::new();
        let mut first_input = base_input(&known, &policy);
        first_input.events = &events;
        let first = detect_triggers(&first_input);
        assert_eq!(first.len(), 1);

        let mut replay_known = HashSet::new();
        replay_known.insert(first[0].dedup_key());
        let mut replay_input = base_input(&replay_known, &policy);
        replay_input.events = &events;
        let second = detect_triggers(&replay_input);
        assert!(
            second.is_empty(),
            "replaying the same event range must not create duplicates"
        );
    }

    #[test]
    fn dedup_within_a_single_pass_keeps_distinct_followups() {
        let followups = vec![
            ReviewerFollowupSnapshot {
                followup_id: "FUP-1".into(),
                task_id: "T-1".into(),
                review_id: Some("REV-1".into()),
                severity: "blocking".into(),
                summary: "first wording".into(),
            },
            ReviewerFollowupSnapshot {
                followup_id: "FUP-2".into(),
                task_id: "T-1".into(),
                review_id: Some("REV-1".into()),
                severity: "blocking".into(),
                summary: "second wording".into(),
            },
        ];
        let known = HashSet::new();
        let policy = TriggerDetectorPolicy::default();
        let mut input = base_input(&known, &policy);
        input.open_followups = &followups;
        let triggers = detect_triggers(&input);
        assert_eq!(
            triggers.len(),
            2,
            "distinct followup ids on the same review must remain distinct"
        );
    }

    #[test]
    fn dedup_within_a_single_pass_collapses_same_followup_id() {
        let followups = vec![
            ReviewerFollowupSnapshot {
                followup_id: "FUP-1".into(),
                task_id: "T-1".into(),
                review_id: Some("REV-1".into()),
                severity: "blocking".into(),
                summary: "first wording".into(),
            },
            ReviewerFollowupSnapshot {
                followup_id: "FUP-1".into(),
                task_id: "T-1".into(),
                review_id: Some("REV-1".into()),
                severity: "blocking".into(),
                summary: "second wording".into(),
            },
        ];
        let known = HashSet::new();
        let policy = TriggerDetectorPolicy::default();
        let mut input = base_input(&known, &policy);
        input.open_followups = &followups;
        let triggers = detect_triggers(&input);
        assert_eq!(triggers.len(), 1);
    }
}
