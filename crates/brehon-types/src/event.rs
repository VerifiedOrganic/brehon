//! Event-sourcing types for the Brehon system.
//!
//! Events are the source of truth. Every state change is captured as an append-only
//! event with a monotonic global sequence number.

use chrono::{DateTime, Utc};
use serde::{de::Error as DeError, Deserialize, Serialize};
use std::fmt;

use crate::feedback::{
    FeedbackOutcomeKind, FeedbackOutcomeRejectionReason, FeedbackTriggerId, FeedbackTriggerKind,
    FeedbackTurnId,
};
use crate::proof::{
    ProofBlocker, ProofBundleId, ProofBundleStatus, ProofCheck, ProofCommand, ProofDecision,
    ProofIntegration, ProofReview,
};
use crate::run::{ClaimGeneration, ClaimOwner, RunId, RunRole, RunStatus};
use crate::task::TaskId;
use crate::SessionId;

/// Delivery lifecycle state of a nudge sent to a stuck worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NudgeDeliveryState {
    /// Nudge was delivered to the worker.
    Delivered,
    /// Worker acknowledged the nudge.
    Acknowledged,
    /// Worker made progress after the nudge.
    ActedOn,
    /// Worker did not acknowledge or act before the nudge timeout.
    TimedOut,
}

impl fmt::Display for NudgeDeliveryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NudgeDeliveryState::Delivered => write!(f, "delivered"),
            NudgeDeliveryState::Acknowledged => write!(f, "acknowledged"),
            NudgeDeliveryState::ActedOn => write!(f, "acted_on"),
            NudgeDeliveryState::TimedOut => write!(f, "timed_out"),
        }
    }
}

/// Unique identifier for an event.
///
/// This is a **monotonic global sequence number**, NOT a timestamp.
/// Events are ordered by their sequence number, which is assigned
/// at append time by the event store.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(pub u64);

impl EventId {
    /// Create a new `EventId` from a sequence number.
    pub fn new(seq: u64) -> Self {
        Self(seq)
    }

    /// Return the inner sequence number.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Correlation ID for tracing related events across boundaries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CorrelationId(pub String);

impl CorrelationId {
    /// Create a new `CorrelationId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Causation ID for tracking what caused an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CausationId(pub String);

impl CausationId {
    /// Create a new `CausationId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Idempotency key for deduplicating events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(pub String);

impl IdempotencyKey {
    /// Create a new `IdempotencyKey` from any string-like value.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

/// Destination bucket for a proof decision event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProofDecisionScope {
    /// Operator decision evidence.
    Operator,
    /// Supervisor decision evidence.
    Supervisor,
}

/// All possible event kinds in the system.
///
/// This enum covers every state transition in the architecture.
/// Each variant captures the relevant context for that event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventKind {
    // === Agent lifecycle ===
    /// Agent session spawned.
    AgentSpawned {
        agent_id: String,
        session_id: String,
        role: String,
    },
    /// Agent session died or was terminated.
    AgentDied {
        agent_id: String,
        session_id: String,
        reason: String,
    },

    // === Prompt/response cycle ===
    /// Prompt sent to agent.
    PromptSent {
        session_id: String,
        prompt_id: String,
        content: String,
    },
    /// Prompt cancelled before completion.
    PromptCancelled {
        session_id: String,
        prompt_id: String,
        reason: String,
    },
    /// Response received from agent.
    ResponseReceived {
        session_id: String,
        prompt_id: String,
        tokens_used: u64,
    },

    // === Permission flow ===
    /// Agent requested permission for an action.
    PermissionRequested {
        session_id: String,
        permission_id: String,
        action: String,
    },
    /// Permission request resolved (approved/denied).
    PermissionResolved {
        session_id: String,
        permission_id: String,
        approved: bool,
    },

    // === Operation tracking ===
    /// Long-running operation started (for stuck detection).
    OperationStarted {
        session_id: String,
        operation: String,
    },
    /// Operation completed.
    OperationCompleted {
        session_id: String,
        operation: String,
        success: bool,
    },

    // === Task lifecycle ===
    /// Task created on the board.
    TaskCreated { task_id: String },
    /// Task assigned to a worker.
    TaskAssigned { task_id: String, agent_id: String },
    /// Task completed (moved to review).
    TaskCompleted { task_id: String },

    // === Run lifecycle ===
    /// Durable run record created.
    RunCreated {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        status: RunStatus,
    },
    /// Durable run claimed by an owner.
    RunClaimed {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        owner: ClaimOwner,
        session_id: Option<SessionId>,
        generation: ClaimGeneration,
        lease_expires_at: DateTime<Utc>,
    },
    /// Durable run claim renewed.
    RunClaimRenewed {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        owner: ClaimOwner,
        generation: ClaimGeneration,
        lease_expires_at: DateTime<Utc>,
    },
    /// Durable run started execution.
    RunStarted {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        generation: ClaimGeneration,
        started_at: DateTime<Utc>,
    },
    /// Durable run reported activity.
    RunActivityObserved {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        generation: ClaimGeneration,
        activity: String,
        observed_at: DateTime<Utc>,
    },
    /// Durable run claim released.
    RunReleased {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        generation: ClaimGeneration,
        reason: Option<String>,
        released_at: DateTime<Utc>,
    },
    /// Durable run queued for retry.
    RunRetryQueued {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        generation: ClaimGeneration,
        reason: String,
        queued_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_at: Option<DateTime<Utc>>,
    },
    /// Durable run completed successfully.
    RunCompleted {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        generation: ClaimGeneration,
        completed_at: DateTime<Utc>,
    },
    /// Durable run failed.
    RunFailed {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        generation: ClaimGeneration,
        reason: String,
        failed_at: DateTime<Utc>,
    },
    /// Durable run abandoned.
    RunAbandoned {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        generation: ClaimGeneration,
        reason: String,
        abandoned_at: DateTime<Utc>,
    },
    /// A stale run mutation was rejected by generation fence.
    StaleRunMutationRejected {
        run_id: RunId,
        task_id: TaskId,
        role: RunRole,
        attempted_generation: ClaimGeneration,
        current_generation: ClaimGeneration,
        mutation: String,
    },

    // === Proof lifecycle ===
    /// Proof bundle created for a task.
    ProofBundleCreated {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        run_ids: Vec<RunId>,
        created_at: DateTime<Utc>,
    },
    /// Command evidence recorded in a proof bundle.
    ProofCommandRecorded {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        command: ProofCommand,
        recorded_at: DateTime<Utc>,
    },
    /// Check or test evidence recorded in a proof bundle.
    ProofCheckRecorded {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        check: ProofCheck,
        is_test_result: bool,
        recorded_at: DateTime<Utc>,
    },
    /// Review evidence linked into a proof bundle.
    ProofReviewLinked {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        review: ProofReview,
        linked_at: DateTime<Utc>,
    },
    /// Integration evidence recorded in a proof bundle.
    ProofIntegrationRecorded {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        integration: ProofIntegration,
        recorded_at: DateTime<Utc>,
    },
    /// Operator or supervisor decision recorded in a proof bundle.
    ProofDecisionRecorded {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        scope: ProofDecisionScope,
        decision: ProofDecision,
        recorded_at: DateTime<Utc>,
    },
    /// Blocker evidence recorded in a proof bundle.
    ProofBlockerRecorded {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        blocker: ProofBlocker,
        recorded_at: DateTime<Utc>,
    },
    /// Proof bundle finalized with an explicit status.
    ProofBundleFinalized {
        proof_bundle_id: ProofBundleId,
        task_id: TaskId,
        final_status: ProofBundleStatus,
        finalized_at: DateTime<Utc>,
    },

    // === Review lifecycle ===
    /// Review requested for a task.
    ReviewRequested { task_id: String, review_id: String },
    /// Review score received from a reviewer.
    ReviewScoreReceived {
        review_id: String,
        reviewer_id: String,
        score: u8,
    },
    /// Review approved (threshold met).
    ReviewApproved { review_id: String },
    /// Review rejected (fundamental issues).
    ReviewRejected { review_id: String },
    /// Changes requested for a review.
    ReviewChangesRequested { review_id: String },

    // === Merge lifecycle ===
    /// Merge prepared (rebase completed in integration worktree).
    MergePrepared { task_id: String, branch: String },
    /// Merge committed to main.
    MergeCommitted { task_id: String },
    /// Merge aborted (conflicts or other issues).
    MergeAborted { task_id: String, reason: String },

    // === Epic/branch lifecycle ===
    /// Epic branch created for collecting subtask integrations.
    EpicBranchCreated {
        epic_id: String,
        branch: String,
        base_commit: String,
    },
    /// Subtask branch created from epic branch.
    SubtaskBranchCreated {
        subtask_id: String,
        branch: String,
        base_branch: String,
    },
    /// Subtask integrated into epic branch.
    SubtaskIntegrated {
        subtask_id: String,
        epic_id: String,
        branch: String,
    },

    // === Supervisor actions ===
    /// Nudge sent to stuck worker.
    NudgeSent {
        session_id: String,
        kind: String,
        content: String,
    },
    /// Nudge was acknowledged by worker (worker emitted an event after nudge).
    NudgeAcknowledged {
        session_id: String,
        nudge_kind: String,
    },
    /// Worker made meaningful progress after nudge (percent change or commit).
    NudgeActedOn {
        session_id: String,
        nudge_kind: String,
        progress_type: String,
    },
    /// Worker did not acknowledge or act on a nudge before timeout.
    NudgeTimedOut {
        session_id: String,
        nudge_id: String,
        nudge_kind: String,
        elapsed_secs: u64,
    },
    /// Memory created by an agent.
    MemoryCreated {
        memory_id: String,
        content: String,
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_agent: Option<String>,
    },
    /// Memory deleted or tombstoned by policy.
    MemoryDeleted { memory_id: String },
    /// Stuck agent detected.
    StuckDetected {
        session_id: String,
        duration_minutes: u64,
        pattern: Option<String>,
    },
    /// Escalation triggered (human intervention needed).
    EscalationTriggered { reason: String, context: String },

    // === Supervisor feedback lifecycle (Phase 6) ===
    /// A new feedback trigger was detected from the event stream.
    FeedbackTriggerDetected {
        trigger_id: FeedbackTriggerId,
        dedup_key: String,
        kind: FeedbackTriggerKind,
        task_id: Option<TaskId>,
        run_id: Option<RunId>,
        review_id: Option<String>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        summary: String,
        detected_at: DateTime<Utc>,
    },
    /// A bounded feedback brief was built for a trigger.
    FeedbackBriefBuilt {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        run_id: Option<RunId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        contract_version: u32,
        total_bytes: usize,
        section_count: usize,
        truncated: bool,
        has_missing_context: bool,
        built_at: DateTime<Utc>,
    },
    /// A supervisor turn started; the supervisor has the brief in hand.
    FeedbackTurnStarted {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        supervisor_id: Option<String>,
        started_at: DateTime<Utc>,
    },
    /// A raw outcome was received from the supervisor; validation pending.
    FeedbackOutcomeReceived {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        outcome_kind: FeedbackOutcomeKind,
        received_at: DateTime<Utc>,
    },
    /// A feedback outcome passed validation.
    FeedbackOutcomeValidated {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        outcome_kind: FeedbackOutcomeKind,
        validated_at: DateTime<Utc>,
    },
    /// A feedback outcome failed validation. The reason is durable.
    FeedbackOutcomeRejected {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        outcome_kind: Option<FeedbackOutcomeKind>,
        reason: FeedbackOutcomeRejectionReason,
        message: String,
        rejected_at: DateTime<Utc>,
    },
    /// A validated feedback decision was recorded.
    FeedbackDecisionRecorded {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        outcome_kind: FeedbackOutcomeKind,
        decided_at: DateTime<Utc>,
        decision_summary: String,
    },
    /// A feedback decision was successfully applied (downstream effect ran).
    FeedbackApplied {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        outcome_kind: FeedbackOutcomeKind,
        application_summary: String,
        applied_at: DateTime<Utc>,
    },
    /// A feedback decision failed to apply (downstream effect errored).
    FeedbackFailed {
        trigger_id: FeedbackTriggerId,
        turn_id: FeedbackTurnId,
        task_id: Option<TaskId>,
        source_event_ids: Vec<EventId>,
        covered_event_range: Option<(EventId, EventId)>,
        outcome_kind: FeedbackOutcomeKind,
        error: String,
        failed_at: DateTime<Utc>,
    },

    // === System state ===
    /// System entering drain mode (shutting down gracefully).
    SystemDraining { reason: String },

    // === Worker reassignment ===
    /// Worker reassigned from one worker to another.
    WorkerReassigned {
        old_worker: String,
        new_worker: String,
        task_id: String,
        reason: String,
        worktree_state: String,
    },
}

/// Core event structure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    /// The event kind and its data.
    pub kind: EventKind,
    /// Wall-clock timestamp.
    pub timestamp: DateTime<Utc>,
    /// Reference to aggregate (task_id, review_id, agent_id, etc.).
    pub aggregate_id: String,
}

/// Full event envelope with metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventEnvelope {
    /// The event itself.
    pub event: Event,
    /// Monotonic sequence number assigned by the event store.
    pub event_id: EventId,
    /// Correlation ID for tracing.
    pub correlation_id: Option<CorrelationId>,
    /// Causation ID for causation tracking.
    pub causation_id: Option<CausationId>,
    /// Idempotency key for deduplication.
    pub idempotency_key: Option<IdempotencyKey>,
}

/// Versioned serialization wrapper for persisted event envelopes.
///
/// Events stored on disk use this outer envelope to preserve a schema
/// version for safe forward/backward migrations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionedEventEnvelope {
    /// Envelope schema version.
    pub v: u32,
    /// Nested unversioned event envelope payload.
    pub envelope: EventEnvelope,
}

impl VersionedEventEnvelope {
    /// Current on-disk event-envelope schema version.
    pub const CURRENT_VERSION: u32 = 1;

    /// Wrap an unversioned envelope in the current schema version.
    pub fn new(envelope: EventEnvelope) -> Self {
        Self {
            v: Self::CURRENT_VERSION,
            envelope,
        }
    }
}

/// Serialize an envelope using the current on-disk versioned format.
pub fn serialize_event_envelope(envelope: &EventEnvelope) -> Result<Vec<u8>, serde_json::Error> {
    let wrapped = VersionedEventEnvelope::new(envelope.clone());
    serde_json::to_vec(&wrapped)
}

/// Deserialize a persisted envelope, supporting both the current versioned
/// format and legacy unversioned payloads.
pub fn deserialize_event_envelope(bytes: &[u8]) -> Result<EventEnvelope, serde_json::Error> {
    let payload: serde_json::Value = serde_json::from_slice(bytes)?;
    if payload.get("v").is_some() {
        let wrapped: VersionedEventEnvelope = serde_json::from_value(payload)?;
        if wrapped.v != VersionedEventEnvelope::CURRENT_VERSION {
            return Err(DeError::custom(format!(
                "unsupported event envelope version: {}",
                wrapped.v
            )));
        }
        Ok(wrapped.envelope)
    } else {
        serde_json::from_value(payload).map_err(serde_json::Error::from)
    }
}
/// Filter for querying events.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventFilter {
    /// Filter by aggregate ID.
    pub aggregate_id: Option<String>,
    /// Filter by event kinds.
    pub kinds: Option<Vec<EventKind>>,
    /// Filter by agent ID.
    pub agent_id: Option<String>,
    /// Filter by task ID.
    pub task_id: Option<String>,
    /// Filter by review ID.
    pub review_id: Option<String>,
    /// Filter by session ID.
    pub session_id: Option<String>,
    /// Filter by timestamp range.
    pub since: Option<DateTime<Utc>>,
    /// Filter by timestamp range.
    pub until: Option<DateTime<Utc>>,
    /// Limit number of results.
    pub limit: Option<usize>,
}

impl EventFilter {
    /// Create an empty filter that matches all events.
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter by aggregate ID.
    pub fn aggregate(mut self, id: impl Into<String>) -> Self {
        self.aggregate_id = Some(id.into());
        self
    }

    /// Filter to a single event kind.
    pub fn kind(mut self, kind: EventKind) -> Self {
        self.kinds = Some(vec![kind]);
        self
    }

    /// Filter to multiple event kinds.
    pub fn kinds(mut self, kinds: Vec<EventKind>) -> Self {
        self.kinds = Some(kinds);
        self
    }

    /// Filter by agent ID.
    pub fn agent(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }

    /// Filter by task ID.
    pub fn task(mut self, id: impl Into<String>) -> Self {
        self.task_id = Some(id.into());
        self
    }

    /// Limit the number of results returned.
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
