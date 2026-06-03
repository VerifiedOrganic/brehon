//! Durable run and claim state types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::{SessionId, TaskId};

/// Unique identifier for a durable run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct RunId(pub String);

impl RunId {
    /// Create a new `RunId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for RunId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            Err("run id cannot be empty")
        } else {
            Ok(Self::new(trimmed))
        }
    }
}

/// Durable role lane for a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
    /// Worker implementation attempt.
    Worker,
    /// Reviewer or review-panel attempt.
    Reviewer,
    /// Supervisor coordination attempt.
    Supervisor,
    /// Integration/merge attempt.
    Integration,
    /// Maintenance or recovery lane.
    Maintenance,
    /// Project-defined lane.
    Custom(String),
}

impl RunRole {
    /// Return the canonical role name.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
            Self::Supervisor => "supervisor",
            Self::Integration => "integration",
            Self::Maintenance => "maintenance",
            Self::Custom(value) => value.as_str(),
        }
    }
}

impl fmt::Display for RunRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for RunRole {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err("run role cannot be empty");
        }

        match trimmed {
            "worker" | "Worker" => Ok(Self::Worker),
            "reviewer" | "Reviewer" => Ok(Self::Reviewer),
            "supervisor" | "Supervisor" => Ok(Self::Supervisor),
            "integration" | "Integration" => Ok(Self::Integration),
            "maintenance" | "Maintenance" => Ok(Self::Maintenance),
            custom => Ok(Self::Custom(custom.to_string())),
        }
    }
}

/// Durable execution state for a run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Run exists but has not been claimed.
    Created,
    /// A live claim exists, but execution has not been observed yet.
    Claimed,
    /// Claimed run has started or reported activity.
    Running,
    /// Claim was released and the run can be claimed again.
    Released,
    /// Run is queued for another attempt.
    RetryQueued,
    /// Run completed successfully.
    Completed,
    /// Run failed.
    Failed,
    /// Run was abandoned and should not be reclaimed.
    Abandoned,
}

impl RunStatus {
    /// Return true when no later mutation should reactivate this run.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Abandoned)
    }

    /// Return true when this status has an active owner lease.
    pub fn has_active_claim(self) -> bool {
        matches!(self, Self::Claimed | Self::Running)
    }

    /// Return the canonical status string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Released => "released",
            Self::RetryQueued => "retry_queued",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for RunStatus {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "created" | "Created" => Ok(Self::Created),
            "claimed" | "Claimed" => Ok(Self::Claimed),
            "running" | "Running" => Ok(Self::Running),
            "released" | "Released" => Ok(Self::Released),
            "retry_queued" | "RetryQueued" => Ok(Self::RetryQueued),
            "completed" | "Completed" => Ok(Self::Completed),
            "failed" | "Failed" => Ok(Self::Failed),
            "abandoned" | "Abandoned" => Ok(Self::Abandoned),
            _ => Err("unknown run status"),
        }
    }
}

/// Durable claim owner identifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ClaimOwner(pub String);

impl ClaimOwner {
    /// Create a new `ClaimOwner` from any string-like value.
    pub fn new(owner: impl Into<String>) -> Self {
        Self(owner.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClaimOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ClaimOwner {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            Err("claim owner cannot be empty")
        } else {
            Ok(Self::new(trimmed))
        }
    }
}

/// Monotonic fence for claim ownership.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Default,
)]
pub struct ClaimGeneration(pub u64);

impl ClaimGeneration {
    /// Create a generation from a raw value.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the raw generation value.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Return the next generation.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for ClaimGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Durable execution run record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunRecord {
    /// Unique run identifier.
    pub run_id: RunId,
    /// Task this run belongs to.
    pub task_id: TaskId,
    /// Role lane this run belongs to.
    pub role: RunRole,
    /// Current durable run status.
    pub status: RunStatus,
    /// Monotonic generation for the current or most recent claim.
    pub claim_generation: ClaimGeneration,
    /// Current claim owner, if claimed.
    pub claim_owner: Option<ClaimOwner>,
    /// Current session, if claimed by a live session.
    pub session_id: Option<SessionId>,
    /// Absolute lease expiration for the current claim.
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Attempt number for this run lane.
    pub attempt: u32,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// When the current claim was created.
    pub claimed_at: Option<DateTime<Utc>>,
    /// When execution started.
    pub started_at: Option<DateTime<Utc>>,
    /// Last observed activity timestamp.
    pub last_activity_at: Option<DateTime<Utc>>,
    /// When a claim was released.
    pub released_at: Option<DateTime<Utc>>,
    /// Release reason, if any.
    pub release_reason: Option<String>,
    /// When the run completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Completion summary, if any.
    pub completion_summary: Option<String>,
    /// When the run failed.
    pub failed_at: Option<DateTime<Utc>>,
    /// Failure reason, if any.
    pub failure_reason: Option<String>,
    /// When this run was queued for retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_queued_at: Option<DateTime<Utc>>,
    /// Earliest time a queued retry may create the next attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
    /// Retry queue reason, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_reason: Option<String>,
    /// Number of continuation prompts sent within this durable run.
    #[serde(default)]
    pub continuation_turns: u32,
    /// Last continuation prompt timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_continuation_at: Option<DateTime<Utc>>,
    /// When the run was abandoned.
    pub abandoned_at: Option<DateTime<Utc>>,
    /// Abandonment reason, if any.
    pub abandoned_reason: Option<String>,
}

impl RunRecord {
    /// Create a new unclaimed run record.
    pub fn new(run_id: RunId, task_id: TaskId, role: RunRole, created_at: DateTime<Utc>) -> Self {
        Self {
            run_id,
            task_id,
            role,
            status: RunStatus::Created,
            claim_generation: ClaimGeneration::default(),
            claim_owner: None,
            session_id: None,
            lease_expires_at: None,
            attempt: 1,
            created_at,
            updated_at: created_at,
            claimed_at: None,
            started_at: None,
            last_activity_at: None,
            released_at: None,
            release_reason: None,
            completed_at: None,
            completion_summary: None,
            failed_at: None,
            failure_reason: None,
            retry_queued_at: None,
            retry_at: None,
            retry_reason: None,
            continuation_turns: 0,
            last_continuation_at: None,
            abandoned_at: None,
            abandoned_reason: None,
        }
    }

    /// Return true if this run is not terminal.
    pub fn is_active(&self) -> bool {
        !self.status.is_terminal()
    }

    /// Return true if the current claim lease has expired at `now`.
    pub fn claim_is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.lease_expires_at
            .is_some_and(|expires_at| expires_at <= now)
    }

    /// Return true if this retry-queued run may create the next attempt.
    pub fn retry_is_due_at(&self, now: DateTime<Utc>) -> bool {
        self.status == RunStatus::RetryQueued
            && self.retry_at.is_none_or(|retry_at| retry_at <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_display_parse_and_serde() {
        let id: RunId = "RUN-123".parse().unwrap();
        assert_eq!(id.as_str(), "RUN-123");
        assert_eq!(id.to_string(), "RUN-123");

        let json = serde_json::to_string(&id).unwrap();
        let parsed: RunId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn run_role_display_parse_and_serde() {
        let role: RunRole = "worker".parse().unwrap();
        assert_eq!(role, RunRole::Worker);
        assert_eq!(role.to_string(), "worker");

        let custom: RunRole = "codex-hardening".parse().unwrap();
        assert_eq!(custom.as_str(), "codex-hardening");

        let json = serde_json::to_string(&RunRole::Integration).unwrap();
        assert_eq!(json, r#""integration""#);
        let parsed: RunRole = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, RunRole::Integration);
    }

    #[test]
    fn run_status_helpers_and_serde() {
        assert!(RunStatus::Completed.is_terminal());
        assert!(!RunStatus::Released.is_terminal());
        assert!(RunStatus::Claimed.has_active_claim());
        assert_eq!(
            "retry_queued".parse::<RunStatus>().unwrap(),
            RunStatus::RetryQueued
        );

        let json = serde_json::to_string(&RunStatus::RetryQueued).unwrap();
        assert_eq!(json, r#""retry_queued""#);
        let parsed: RunStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, RunStatus::RetryQueued);
    }

    #[test]
    fn claim_owner_and_generation_helpers() {
        let owner: ClaimOwner = "worker-1".parse().unwrap();
        assert_eq!(owner.as_str(), "worker-1");

        let generation = ClaimGeneration::new(41).next();
        assert_eq!(generation.as_u64(), 42);
        assert_eq!(generation.to_string(), "42");
    }

    #[test]
    fn run_record_round_trip() {
        let now = Utc::now();
        let record = RunRecord::new(
            RunId::new("RUN-1"),
            TaskId::new("T-1"),
            RunRole::Worker,
            now,
        );

        assert!(record.is_active());
        assert!(!record.claim_is_expired_at(now));

        let json = serde_json::to_string(&record).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, record);
    }
}
