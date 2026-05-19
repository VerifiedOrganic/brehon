//! Materialized view types for state projection.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::review::ReviewStatus;
use crate::run::RunRecord;
use crate::task::TaskStatus;

/// Unique identifier for a queue claim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ClaimId(pub String);

impl ClaimId {
    /// Create a new `ClaimId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClaimId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Materialized view of task state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskView {
    /// Task ID.
    pub task_id: String,
    /// Current status.
    pub status: TaskStatus,
    /// Assigned agent (if any).
    pub assignee: Option<String>,
    /// Session ID (if active).
    pub session_id: Option<String>,
    /// Branch name (if any).
    pub branch: Option<String>,
    /// Number of review rounds.
    pub review_rounds: u32,
    /// Last seen event ID.
    pub last_event_id: u64,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Materialized view of review state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewView {
    /// Review ID.
    pub review_id: String,
    /// Task being reviewed.
    pub task_id: String,
    /// Current status.
    pub status: ReviewStatus,
    /// Review round number.
    pub round: u32,
    /// Scores collected.
    pub scores: Vec<(String, u8)>,
    /// Reviewer panel.
    pub panel: Vec<String>,
    /// Last seen event ID.
    pub last_event_id: u64,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Materialized view of durable run state for operator surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunStateView {
    /// Run ID.
    pub run_id: String,
    /// Task this run belongs to.
    pub task_id: String,
    /// Durable role lane.
    pub role: String,
    /// Attempt number in this lane.
    pub attempt: u32,
    /// Current durable status.
    pub status: String,
    /// Current claim owner, if claimed.
    pub owner: Option<String>,
    /// Current session, if claimed by a live session.
    pub session: Option<String>,
    /// Last observed activity timestamp.
    pub last_activity: Option<DateTime<Utc>>,
    /// Current claim lease expiry.
    pub lease_expiry: Option<DateTime<Utc>>,
    /// Earliest retry time for retry-queued runs.
    pub retry_at: Option<DateTime<Utc>>,
    /// Last durable mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// True when an active claim lease is expired at projection time.
    pub stale: bool,
}

impl RunStateView {
    /// Project a durable run record at a deterministic observation time.
    pub fn from_run_record_at(record: &RunRecord, now: DateTime<Utc>) -> Self {
        Self {
            run_id: record.run_id.to_string(),
            task_id: record.task_id.to_string(),
            role: record.role.to_string(),
            attempt: record.attempt,
            status: record.status.to_string(),
            owner: record.claim_owner.as_ref().map(ToString::to_string),
            session: record.session_id.as_ref().map(ToString::to_string),
            last_activity: record.last_activity_at,
            lease_expiry: record.lease_expires_at,
            retry_at: record.retry_at,
            updated_at: record.updated_at,
            stale: record.status.has_active_claim() && record.claim_is_expired_at(now),
        }
    }
}

impl From<&RunRecord> for RunStateView {
    fn from(record: &RunRecord) -> Self {
        Self::from_run_record_at(record, Utc::now())
    }
}

/// Claim on a queue item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueClaim {
    /// Claim ID.
    pub claim_id: ClaimId,
    /// Queue name.
    pub queue: String,
    /// Item being claimed.
    pub item_id: String,
    /// Consumer who claimed it.
    pub consumer: String,
    /// When claim expires.
    pub expires_at: DateTime<Utc>,
    /// Lease epoch identifier used for durable cross-restart validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_epoch: Option<String>,
    /// Requested lease duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_duration_ms: Option<u64>,
    /// Monotonic deadline (milliseconds since epoch start) for in-process expiry checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monotonic_deadline_ms: Option<u64>,
}

impl QueueClaim {
    /// Return `true` if this claim has passed its expiration time.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

/// Update to a materialized view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewUpdate {
    /// View type.
    pub view_type: ViewType,
    /// View key (e.g., task_id).
    pub key: String,
    /// Update operation.
    pub operation: ViewOperation,
}

/// Type of materialized view.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ViewType {
    /// Task board view.
    Task,
    /// Review state view.
    Review,
    /// Agent session view.
    Agent,
    /// Budget tracking view.
    Budget,
}

/// View update operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ViewOperation {
    /// Set field value.
    Set { field: String, value: String },
    /// Increment numeric field.
    Increment { field: String, amount: i64 },
    /// Append to array field.
    Append { field: String, value: String },
    /// Remove from array field.
    Remove { field: String, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_id_display() {
        let id = ClaimId::new("claim-123");
        assert_eq!(format!("{}", id), "claim-123");
    }

    #[test]
    fn queue_claim_expiry() {
        let claim = QueueClaim {
            claim_id: ClaimId::new("c1"),
            queue: "review:high".into(),
            item_id: "T001".into(),
            consumer: "reviewer-1".into(),
            expires_at: Utc::now() - chrono::Duration::seconds(1),
            lease_epoch: None,
            lease_duration_ms: None,
            monotonic_deadline_ms: None,
        };
        assert!(claim.is_expired());
    }

    #[test]
    fn view_update_serialization() {
        let update = ViewUpdate {
            view_type: ViewType::Task,
            key: "T001".into(),
            operation: ViewOperation::Set {
                field: "status".into(),
                value: "InProgress".into(),
            },
        };
        let json = serde_json::to_string(&update).unwrap();
        let parsed: ViewUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(update, parsed);
    }

    #[test]
    fn task_view() {
        let view = TaskView {
            task_id: "T001".into(),
            status: TaskStatus::InProgress,
            assignee: Some("agent-1".into()),
            session_id: Some("sess-1".into()),
            branch: Some("brehon/T001".into()),
            review_rounds: 0,
            last_event_id: 42,
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&view).unwrap();
        let parsed: TaskView = serde_json::from_str(&json).unwrap();
        assert_eq!(view, parsed);
    }

    #[test]
    fn run_state_view_from_record_includes_claim_retry_and_stale_fields() {
        use crate::run::{ClaimGeneration, ClaimOwner, RunId, RunRecord, RunRole, RunStatus};
        use crate::{SessionId, TaskId};

        let now = DateTime::parse_from_rfc3339("2026-05-16T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut record = RunRecord::new(
            RunId::new("RUN-1"),
            TaskId::new("T-1"),
            RunRole::Worker,
            now - chrono::Duration::minutes(10),
        );
        record.status = RunStatus::Running;
        record.claim_generation = ClaimGeneration::new(2);
        record.claim_owner = Some(ClaimOwner::new("worker-1"));
        record.session_id = Some(SessionId::new("session-1"));
        record.lease_expires_at = Some(now - chrono::Duration::seconds(1));
        record.last_activity_at = Some(now - chrono::Duration::minutes(1));
        record.retry_at = Some(now + chrono::Duration::minutes(5));
        record.updated_at = now;
        record.attempt = 3;

        let view = RunStateView::from_run_record_at(&record, now);

        assert_eq!(view.run_id, "RUN-1");
        assert_eq!(view.task_id, "T-1");
        assert_eq!(view.role, "worker");
        assert_eq!(view.status, "running");
        assert_eq!(view.attempt, 3);
        assert_eq!(view.owner.as_deref(), Some("worker-1"));
        assert_eq!(view.session.as_deref(), Some("session-1"));
        assert_eq!(view.retry_at, record.retry_at);
        assert!(view.stale);
    }
}
