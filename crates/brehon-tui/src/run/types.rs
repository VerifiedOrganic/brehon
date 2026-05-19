//! Public and internal types used across the TUI run loop.

use brehon_types::task::normalize_task_status;
use chrono::{DateTime, Utc};
use crossterm::event::MouseEvent;
use ratatui::layout::Rect;
use std::time::Instant;

// ── Public types ────────────────────────────────────────────────────────────

/// A reviewer panel groups reviewer pane IDs that belong to the same pool.
#[derive(Debug, Clone)]
pub struct ReviewerPanel {
    /// Panel name (e.g. "codex", "claude-code")
    pub name: String,
    /// Pane IDs belonging to this panel
    pub members: Vec<String>,
}

/// Dashboard data for the overview tab.
///
/// Populated from Mux panes and optionally enriched by polling session files
/// written by MCP server processes.
#[derive(Debug, Clone, Default)]
pub struct DashboardData {
    /// Active agents known to the session.
    pub agents: Vec<AgentInfo>,
    /// All tasks tracked by the session.
    pub tasks: Vec<TaskInfo>,
    /// Recent activity events (newest first).
    pub events: Vec<EventInfo>,
    /// Path to `.brehon` root — used to read `runtime/sessions/*.json`.
    pub brehon_root: Option<std::path::PathBuf>,
}

/// Agent status as visible to the dashboard.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    /// Agent display name.
    pub name: String,
    /// Role (e.g. "supervisor", "worker").
    pub role: String,
    /// CLI binary used (e.g. "claude", "codex").
    pub cli: String,
    /// MCP session ID (set after agent calls `session_start`).
    pub session_id: Option<String>,
    /// Timestamp when the agent last refreshed its MCP session file.
    pub last_seen_at: Option<String>,
}

/// Comprehensive snapshot of a task for dashboard display.
#[derive(Debug, Clone)]
pub struct TaskInfo {
    /// Unique task identifier.
    pub id: String,
    /// Short task title.
    pub title: String,
    /// Current status string (e.g. "open", "in_progress", "merged").
    pub status: String,
    pub assignee: Option<String>,
    pub task_type: String,
    pub parent_id: Option<String>,
    pub description: String,
    pub priority: Option<String>,
    pub percent: Option<u64>,
    pub tokens_used: u64,
    pub completion_mode: Option<String>,
    pub merge_target: Option<String>,
    pub integration_status: Option<String>,
    pub integration_branch: Option<String>,
    pub integration_worktree: Option<String>,
    pub activity: Option<String>,
    pub notes: Option<String>,
    pub blockers: Option<String>,
    pub dependencies: Vec<String>,
    pub blocked_by: Vec<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub closed_at: Option<String>,
    pub closed_by: Option<String>,
    pub merged_commit: Option<String>,
    pub merged_branch: Option<String>,
    /// HEAD commit recorded by the most recent `task action=checkpoint` or
    /// `action=complete`. Used by stall detection to distinguish
    /// "worker went idle after checkpointing" (nudge candidate) from
    /// "worker went idle without ever checkpointing" (plain recycle).
    pub latest_commit: Option<String>,
    pub run: Option<TaskRunInfo>,
    pub review_id: Option<String>,
    pub review_status: Option<String>,
    pub review_round: Option<u64>,
    pub review_panel_id: Option<String>,
    pub review_panel_members: Vec<String>,
    pub review_panel_lease_state: Option<String>,
    pub review_feedback_outcome: Option<String>,
    pub review_feedback_threshold_reason: Option<String>,
    pub review_feedback_evaluated_at: Option<String>,
    pub review_feedback_blocking: Vec<String>,
    pub review_feedback_suggestions: Vec<String>,
    pub review_feedback_nitpicks: Vec<String>,
    pub review_feedback_dissent: Vec<String>,
    pub integration_conflict_owner: Option<String>,
    pub integration_conflict_source: Option<String>,
    pub integration_conflict_merge_target: Option<String>,
    pub integration_conflict_reviewed_commit: Option<String>,
    pub integration_conflict_previous_worker: Option<String>,
    pub integration_conflict_conflicting_files: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub file_hints: Vec<String>,
    pub constraints: Vec<String>,
    pub test_requirements: Vec<String>,
    pub plan_steps: Vec<String>,
    pub implementation_notes: Option<String>,
    pub research_context: Vec<ResearchContextInfo>,
    /// Compact proof bundle summary, mirrored into
    /// `.brehon/runtime/proof/{task_id}.json` by the MCP proof recorders.
    pub proof: Option<brehon_types::ProofSummary>,
    /// Compact supervisor feedback summary, mirrored into
    /// `.brehon/runtime/feedback/{task_id}.json` by the supervisor
    /// feedback cache writer.
    pub feedback: Option<brehon_types::FeedbackTaskSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchContextInfo {
    pub artifact_id: String,
    pub role: String,
    pub title: String,
    pub summary: String,
    pub artifact_path: Option<String>,
    pub confidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRunInfo {
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub role: Option<String>,
    pub status: String,
    pub owner: Option<String>,
    pub session: Option<String>,
    pub attempt: Option<u32>,
    pub max_attempts: Option<u32>,
    pub last_activity_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub retry_at: Option<String>,
    pub retry_reason: Option<String>,
    pub failure_reason: Option<String>,
    pub updated_at: Option<String>,
    pub state_source: Option<String>,
    pub continuation_turns: Option<u32>,
    pub retry_exhausted: bool,
    pub pending_confirmation: bool,
    pub stale: bool,
}

impl TaskRunInfo {
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "claimed" | "running")
    }

    pub fn is_retry_queued(&self) -> bool {
        self.status == "retry_queued"
    }

    pub fn is_failed(&self) -> bool {
        self.status == "failed"
    }

    pub fn retry_exhausted(&self) -> bool {
        self.retry_exhausted
            || (self.is_failed()
                && self
                    .attempt
                    .zip(self.max_attempts)
                    .is_some_and(|(attempt, max)| attempt >= max))
    }

    pub fn claim_is_stale_at(&self, now: DateTime<Utc>) -> bool {
        self.stale
            || (self.is_active()
                && parse_rfc3339_utc(self.lease_expires_at.as_deref())
                    .is_some_and(|lease_expires_at| lease_expires_at <= now))
    }

    pub fn confirmation_label_at(&self, now: DateTime<Utc>) -> &'static str {
        if self.pending_confirmation {
            "pending confirmation"
        } else if self.claim_is_stale_at(now) {
            "stale projection"
        } else {
            "confirmed projection"
        }
    }

    pub fn display_status(&self) -> Option<&'static str> {
        if self.is_retry_queued() {
            Some("retry_queued")
        } else if self.retry_exhausted() {
            Some("retry_exhausted")
        } else if self.is_failed() {
            Some("run_failed")
        } else if self.is_active() {
            Some("active_run")
        } else {
            None
        }
    }

    pub fn dashboard_hint(&self, now: DateTime<Utc>) -> String {
        let mut parts = Vec::new();
        parts.push(match self.display_status() {
            Some("retry_queued") => "retry queued".to_string(),
            Some("retry_exhausted") => "retry exhausted".to_string(),
            Some("run_failed") => "run failed".to_string(),
            Some("active_run") => "active run".to_string(),
            _ => format!("run {}", self.status),
        });
        if let Some(attempt) = self.attempt {
            let attempt = self
                .max_attempts
                .map_or_else(|| attempt.to_string(), |max| format!("{attempt}/{max}"));
            parts.push(format!("attempt {attempt}"));
        }
        if let Some(turns) = self.continuation_turns.filter(|turns| *turns > 0) {
            parts.push(format!("cont {turns}"));
        }
        if self.is_retry_queued() {
            parts.push(format_retry_delay(now, self.retry_at.as_deref()));
        }
        if self.pending_confirmation || self.claim_is_stale_at(now) {
            parts.push(self.confirmation_label_at(now).to_string());
        }
        if let Some(reason) = self
            .retry_reason
            .as_deref()
            .or(self.failure_reason.as_deref())
        {
            let reason = reason.trim();
            if !reason.is_empty() {
                parts.push(reason.to_string());
            }
        }
        parts.join(" | ")
    }
}

pub(crate) fn read_task_run_info(task: &serde_json::Value) -> Option<TaskRunInfo> {
    let nested = task
        .get("run")
        .or_else(|| task.get("run_state"))
        .or_else(|| task.get("active_run"));
    let source = nested.unwrap_or(task);
    let status = if nested.is_some() {
        read_json_string(source, "status").or_else(|| read_json_string(source, "run_status"))
    } else {
        read_json_string(task, "run_status")
    }?;
    Some(TaskRunInfo {
        run_id: read_json_string(source, "run_id").or_else(|| read_json_string(task, "run_id")),
        task_id: read_json_string(source, "task_id")
            .or_else(|| read_json_string(task, "task_id"))
            .or_else(|| read_json_string(task, "id")),
        role: read_json_string(source, "role").or_else(|| read_json_string(task, "run_role")),
        status: normalize_run_status(&status),
        owner: read_json_string(source, "owner")
            .or_else(|| read_json_string(source, "claim_owner"))
            .or_else(|| read_json_string(task, "claim_owner"))
            .or_else(|| read_json_string(task, "assignee")),
        session: read_json_string(source, "session")
            .or_else(|| read_json_string(source, "session_id"))
            .or_else(|| read_json_string(task, "session_id")),
        attempt: read_json_u32(source, "attempt")
            .or_else(|| read_json_u32(source, "run_attempt"))
            .or_else(|| read_json_u32(task, "run_attempt")),
        max_attempts: read_json_u32(source, "max_attempts")
            .or_else(|| read_json_u32(task, "max_attempts")),
        last_activity_at: read_json_string(source, "last_activity_at")
            .or_else(|| read_json_string(source, "last_activity"))
            .or_else(|| read_json_string(task, "last_activity_at")),
        lease_expires_at: read_json_string(source, "lease_expires_at")
            .or_else(|| read_json_string(source, "lease_expiry"))
            .or_else(|| read_json_string(task, "lease_expires_at")),
        retry_at: read_json_string(source, "retry_at")
            .or_else(|| read_json_string(task, "retry_at")),
        retry_reason: read_json_string(source, "retry_reason")
            .or_else(|| read_json_string(task, "retry_reason")),
        failure_reason: read_json_string(source, "failure_reason")
            .or_else(|| read_json_string(task, "failure_reason")),
        updated_at: read_json_string(source, "updated_at")
            .or_else(|| read_json_string(task, "updated_at")),
        state_source: read_json_string(source, "state_source")
            .or_else(|| read_json_string(source, "source"))
            .or_else(|| nested.map(|_| "durable projection".to_string())),
        continuation_turns: read_json_u32(source, "continuation_turns")
            .or_else(|| read_json_u32(task, "continuation_turns")),
        retry_exhausted: read_json_bool(source, "retry_exhausted")
            || read_json_bool(task, "retry_exhausted"),
        pending_confirmation: read_json_bool(source, "pending_confirmation")
            || read_json_bool(task, "pending_confirmation"),
        stale: read_json_bool(source, "stale") || read_json_bool(task, "stale"),
    })
}

fn parse_rfc3339_utc(value: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

fn read_json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
        Some(serde_json::Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

fn read_json_u32(value: &serde_json::Value, key: &str) -> Option<u32> {
    match value.get(key) {
        Some(serde_json::Value::Number(number)) => {
            number.as_u64().and_then(|n| u32::try_from(n).ok())
        }
        Some(serde_json::Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

fn read_json_bool(value: &serde_json::Value, key: &str) -> bool {
    match value.get(key) {
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::String(text)) => text == "true",
        _ => false,
    }
}

fn normalize_run_status(status: &str) -> String {
    let normalized = status.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "retryqueued" | "retry-queued" => "retry_queued".to_string(),
        "inprogress" | "in_progress" => "running".to_string(),
        other => other.to_string(),
    }
}

fn format_retry_delay(now: DateTime<Utc>, retry_at: Option<&str>) -> String {
    let Some(retry_at) = retry_at else {
        return "retry pending".to_string();
    };
    let Ok(parsed) = DateTime::parse_from_rfc3339(retry_at) else {
        return format!("retry at {retry_at}");
    };
    let retry_at = parsed.with_timezone(&Utc);
    if retry_at <= now {
        return "retry due".to_string();
    }
    let seconds = (retry_at - now).num_seconds();
    if seconds < 60 {
        format!("retry in {seconds}s")
    } else if seconds < 3600 {
        format!("retry in {}m", (seconds + 59) / 60)
    } else {
        format!("retry in {}h", (seconds + 3599) / 3600)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReviewContextSnapshot {
    pub review_id: Option<String>,
    pub review_status: Option<String>,
    pub review_round: Option<u64>,
    pub review_panel_id: Option<String>,
    pub review_panel_members: Vec<String>,
    pub has_lease: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingReviewObligation {
    pub task_id: String,
    pub task_title: String,
    pub review_id: String,
    pub panel_id: Option<String>,
    pub round: Option<u64>,
    pub pending_reviewers: usize,
}

/// A timestamped activity event shown in the dashboard.
#[derive(Debug, Clone)]
pub struct EventInfo {
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Human-readable event description.
    pub description: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StalledRecoveryOutcome {
    Reassigned {
        task_id: String,
        old_worker: String,
        new_worker: String,
    },
    ReviewReady {
        task_id: String,
        worker: String,
    },
    Requeued {
        task_id: String,
        old_worker: String,
    },
    Blocked {
        task_id: String,
        worker: String,
        reason: String,
    },
    SupervisorConflict {
        task_id: String,
        worker: String,
        files: Vec<String>,
    },
}

#[allow(dead_code)]
pub(crate) enum WorkerWorktreeInspection {
    Missing,
    Clean,
    Dirty(String),
    Unmerged { files: Vec<String> },
}

pub(crate) fn task_is_terminal(task: &TaskInfo) -> bool {
    normalize_task_status(&task.status).is_some_and(|status| matches!(status, "merged" | "closed"))
}

pub(crate) fn task_is_container(task: &TaskInfo) -> bool {
    matches!(task.task_type.as_str(), "initiative" | "epic")
}

// ── Group tab state ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupTab {
    Dashboard,
    Runtime,
    Advisors,
    Research,
    Workers,
    Reviewers,
}

// ── Click region tracking ───────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClickTarget {
    GroupTab(GroupTab),
    SubTab(String),
    MemberTab(String),
    ResetPane(String),
    SupervisorPane,
    LeftPane,
    EpicToggle(String),
    TaskDetail(String),
    ActivityRow {
        pane_id: String,
        entry_key: String,
    },
    RuntimeApproval {
        approval_id: String,
        session_id: String,
        approved: bool,
    },
}

pub(crate) struct ClickRegion {
    pub rect: Rect,
    pub target: ClickTarget,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskDetailState {
    pub task_id: String,
    pub scroll: u16,
    pub max_scroll: u16,
    pub area: Rect,
}

impl TaskDetailState {
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            scroll: 0,
            max_scroll: 0,
            area: Rect::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct KeybindOverlayState {
    pub area: Rect,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ComposerWorkflow {
    #[default]
    Discover,
    BreakDown,
    Dispatch,
    Recover,
    Message,
}

impl ComposerWorkflow {
    pub(crate) const ALL: [Self; 5] = [
        Self::Discover,
        Self::BreakDown,
        Self::Dispatch,
        Self::Recover,
        Self::Message,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Discover => "Discover",
            Self::BreakDown => "Break down",
            Self::Dispatch => "Dispatch",
            Self::Recover => "Recover",
            Self::Message => "Message",
        }
    }

    pub(crate) fn skill(self) -> Option<&'static str> {
        match self {
            Self::Discover => Some("brehon-discovery"),
            Self::BreakDown => Some("brehon-breakdown"),
            Self::Dispatch => Some("brehon-dispatch"),
            Self::Recover => Some("brehon-supervisor-checklist"),
            Self::Message => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComposerState {
    pub workflow: ComposerWorkflow,
    pub target: String,
    pub task_id: Option<String>,
    pub text: String,
    pub cursor: usize,
    pub status: Option<String>,
    pub area: Rect,
    pub mention_candidates: Vec<String>,
    pub mention_completion: Option<MentionCompletionState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MentionCompletionState {
    pub token_start: usize,
    pub matches: Vec<String>,
    pub selected: usize,
}

impl ComposerState {
    pub(crate) fn new(target: impl Into<String>, task_id: Option<String>) -> Self {
        Self {
            workflow: ComposerWorkflow::default(),
            target: target.into(),
            task_id,
            text: String::new(),
            cursor: 0,
            status: None,
            area: Rect::default(),
            mention_candidates: Vec::new(),
            mention_completion: None,
        }
    }

    pub(crate) fn new_advisor(room_id: impl Into<String>) -> Self {
        let room_id = room_id.into();
        Self {
            workflow: ComposerWorkflow::Message,
            target: format!("advisor:{room_id}"),
            task_id: None,
            text: String::new(),
            cursor: 0,
            status: Some(
                "Post to advisor room. @worker Tab complete, Enter send, Ctrl-j newline, Esc close."
                    .to_string(),
            ),
            area: Rect::default(),
            mention_candidates: Vec::new(),
            mention_completion: None,
        }
    }

    pub(crate) fn new_research(task_id: Option<String>) -> Self {
        let target = task_id
            .as_deref()
            .map(|task_id| format!("research:{task_id}"))
            .unwrap_or_else(|| "research:".to_string());
        let status = if let Some(task_id) = task_id {
            format!("Request research for {task_id}. Enter send, Ctrl-j newline, Esc close.")
        } else {
            "Request research. Use `/task T-123 <request>`, Enter send, Ctrl-j newline, Esc close."
                .to_string()
        };
        Self {
            workflow: ComposerWorkflow::Message,
            target,
            task_id: None,
            text: String::new(),
            cursor: 0,
            status: Some(status),
            area: Rect::default(),
            mention_candidates: Vec::new(),
            mention_completion: None,
        }
    }

    pub(crate) fn with_mention_candidates(mut self, candidates: Vec<String>) -> Self {
        self.mention_candidates = candidates;
        self
    }

    pub(crate) fn advisor_room_id(&self) -> Option<&str> {
        self.target.strip_prefix("advisor:")
    }

    pub(crate) fn is_research_room(&self) -> bool {
        self.target.starts_with("research:")
    }

    pub(crate) fn research_task_id(&self) -> Option<&str> {
        self.target
            .strip_prefix("research:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) enum InputMode {
    #[default]
    Normal,
    KeybindOverlay(KeybindOverlayState),
    Composer(ComposerState),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DashboardTaskListState {
    pub scroll: u16,
    pub max_scroll: u16,
    pub area: Rect,
    pub known_container_ids: std::collections::HashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AdvisorRoomViewState {
    pub scroll: u16,
    pub max_scroll: u16,
    pub area: Rect,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ResearchRoomViewState {
    pub scroll: u16,
    pub max_scroll: u16,
    pub area: Rect,
    pub selected_task_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DashboardAgentListState {
    pub scroll: u16,
    pub max_scroll: u16,
    pub area: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeCommandActivity {
    pub command_id: String,
    pub label: String,
    pub target: Option<String>,
    pub status: String,
    pub message: Option<String>,
    pub issued_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct DashboardTaskRowRegion {
    pub line_idx: u16,
    pub x: u16,
    pub width: u16,
    pub target: ClickTarget,
}

// ── Selection types ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct PanePos {
    pub col: u16,
    pub row: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SelectionPane {
    Left,
    Supervisor,
}

#[derive(Clone, Debug)]
pub(crate) struct SelectionState {
    pub pane: SelectionPane,
    pub pane_id: String,
    pub anchor: PanePos,
    pub extent: PanePos,
}

impl SelectionState {
    /// Return (start, end) in document order (top-to-bottom, left-to-right).
    pub fn ordered(&self) -> (&PanePos, &PanePos) {
        if (self.anchor.row, self.anchor.col) <= (self.extent.row, self.extent.col) {
            (&self.anchor, &self.extent)
        } else {
            (&self.extent, &self.anchor)
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingEscapeSequence {
    pub started_at: Instant,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawSgrMouseParse {
    Incomplete,
    Complete(MouseEvent),
    Invalid,
}

// ── Layout areas ────────────────────────────────────────────────────────────

pub(crate) struct LayoutAreas {
    pub group_tab_bar: Rect,
    pub left_tab_stack: Rect, // area for sub-tabs (and panel tabs for reviewers)
    pub left_content: Rect,
    pub supervisor_area: Rect,
    pub status_bar: Rect,
}

pub(crate) struct TabEntry {
    pub id: String,
    pub label: String,
    pub is_selected: bool,
}

// ── Theme ───────────────────────────────────────────────────────────────────

pub(crate) const REVIEW_FINDINGS_SUMMARY_MAX_CHARS: usize = 160;

pub(crate) const DASH_SECTION_BORDER: ratatui::style::Color = crate::theme::chrome::RULE;
pub(crate) const DASH_ACCENT: ratatui::style::Color = crate::theme::detail::MUTED_ACCENT;

pub(crate) const SESSION_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);
pub(crate) const RAW_ESCAPE_SEQUENCE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(40);
pub(crate) const STALE_ACTIVE_TOOL_THRESHOLD: std::time::Duration =
    std::time::Duration::from_secs(600);

#[cfg(test)]
mod run_projection_tests {
    use super::*;
    use crate::run::task_detail::{compute_display_status, task_dashboard_hint};
    use std::collections::HashMap;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-16T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn retry_queued_run_badge_shows_attempt_delay_and_reason() {
        let task = serde_json::json!({
            "run": {
                "run_id": "run-1",
                "status": "retry_queued",
                "attempt": 1,
                "max_attempts": 2,
                "retry_at": "2026-05-16T12:05:00Z",
                "retry_reason": "session interrupted"
            }
        });
        let run = read_task_run_info(&task).unwrap();

        assert_eq!(run.display_status(), Some("retry_queued"));
        assert_eq!(
            run.dashboard_hint(fixed_now()),
            "retry queued | attempt 1/2 | retry in 5m | session interrupted"
        );
    }

    #[test]
    fn failed_run_badge_shows_retry_exhaustion() {
        let task = serde_json::json!({
            "run_status": "failed",
            "run_attempt": 2,
            "max_attempts": 2,
            "failure_reason": "max attempts reached"
        });
        let run = read_task_run_info(&task).unwrap();

        assert_eq!(run.display_status(), Some("retry_exhausted"));
        assert_eq!(
            run.dashboard_hint(fixed_now()),
            "retry exhausted | attempt 2/2 | max attempts reached"
        );
    }

    #[test]
    fn active_run_badge_shows_continuation_turns() {
        let task = serde_json::json!({
            "active_run": {
                "status": "running",
                "attempt": 2,
                "continuation_turns": 3
            }
        });
        let run = read_task_run_info(&task).unwrap();

        assert_eq!(run.display_status(), Some("active_run"));
        assert_eq!(
            run.dashboard_hint(fixed_now()),
            "active run | attempt 2 | cont 3"
        );
    }

    #[test]
    fn ordinary_task_status_is_not_treated_as_run_status() {
        let task = serde_json::json!({
            "status": "in_progress",
            "task_id": "T-ordinary"
        });

        assert!(read_task_run_info(&task).is_none());
    }

    #[test]
    fn task_display_status_and_hint_surface_run_waiting_state() {
        let run = read_task_run_info(&serde_json::json!({
            "run": {
                "status": "retry_queued",
                "attempt": 1,
                "max_attempts": 2,
                "retry_at": "2026-05-16T12:05:00Z"
            }
        }))
        .unwrap();
        let task = minimal_task(Some(run));

        assert_eq!(compute_display_status(&task), "retry_queued");
        assert!(task_dashboard_hint(&task, &HashMap::new())
            .unwrap()
            .contains("retry queued"));
    }

    #[test]
    fn run_state_reader_surfaces_claim_session_and_timing_fields() {
        let run = read_task_run_info(&serde_json::json!({
            "id": "T-claim",
            "active_run": {
                "run_id": "RUN-claim",
                "task_id": "T-claim",
                "role": "worker",
                "status": "running",
                "attempt": 2,
                "claim_owner": "worker-1",
                "session_id": "session-1",
                "last_activity_at": "2026-05-16T12:01:00Z",
                "lease_expires_at": "2026-05-16T12:10:00Z",
                "retry_at": "2026-05-16T12:15:00Z",
                "updated_at": "2026-05-16T12:02:00Z",
                "source": "durable run projection"
            }
        }))
        .unwrap();

        assert_eq!(run.run_id.as_deref(), Some("RUN-claim"));
        assert_eq!(run.task_id.as_deref(), Some("T-claim"));
        assert_eq!(run.owner.as_deref(), Some("worker-1"));
        assert_eq!(run.session.as_deref(), Some("session-1"));
        assert_eq!(
            run.lease_expires_at.as_deref(),
            Some("2026-05-16T12:10:00Z")
        );
        assert_eq!(run.state_source.as_deref(), Some("durable run projection"));
    }

    #[test]
    fn run_state_stale_claim_uses_expired_lease_or_projection_flag() {
        let mut run = read_task_run_info(&serde_json::json!({
            "run": {
                "status": "running",
                "lease_expires_at": "2026-05-16T11:59:00Z"
            }
        }))
        .unwrap();
        assert!(run.claim_is_stale_at(fixed_now()));
        assert_eq!(run.confirmation_label_at(fixed_now()), "stale projection");

        run.lease_expires_at = Some("2026-05-16T12:10:00Z".to_string());
        run.stale = true;
        assert!(run.dashboard_hint(fixed_now()).contains("stale projection"));
    }

    #[test]
    fn run_state_pending_confirmation_is_visible_in_dashboard_hint() {
        let run = read_task_run_info(&serde_json::json!({
            "run": {
                "status": "claimed",
                "pending_confirmation": true
            }
        }))
        .unwrap();

        assert_eq!(
            run.confirmation_label_at(fixed_now()),
            "pending confirmation"
        );
        assert!(run
            .dashboard_hint(fixed_now())
            .contains("pending confirmation"));
    }

    fn minimal_task(run: Option<TaskRunInfo>) -> TaskInfo {
        TaskInfo {
            id: "T-run".to_string(),
            title: "Run task".to_string(),
            status: "in_progress".to_string(),
            assignee: Some("worker-1".to_string()),
            task_type: "task".to_string(),
            parent_id: None,
            description: String::new(),
            priority: None,
            percent: None,
            tokens_used: 0,
            completion_mode: None,
            merge_target: None,
            integration_status: None,
            integration_branch: None,
            integration_worktree: None,
            activity: None,
            notes: None,
            blockers: None,
            dependencies: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
            closed_at: None,
            closed_by: None,
            merged_commit: None,
            merged_branch: None,
            latest_commit: None,
            run,
            review_id: None,
            review_status: None,
            review_round: None,
            review_panel_id: None,
            review_panel_members: Vec::new(),
            review_panel_lease_state: None,
            review_feedback_outcome: None,
            review_feedback_threshold_reason: None,
            review_feedback_evaluated_at: None,
            review_feedback_blocking: Vec::new(),
            review_feedback_suggestions: Vec::new(),
            review_feedback_nitpicks: Vec::new(),
            review_feedback_dissent: Vec::new(),
            integration_conflict_owner: None,
            integration_conflict_source: None,
            integration_conflict_merge_target: None,
            integration_conflict_reviewed_commit: None,
            integration_conflict_previous_worker: None,
            integration_conflict_conflicting_files: Vec::new(),
            acceptance_criteria: Vec::new(),
            file_hints: Vec::new(),
            constraints: Vec::new(),
            test_requirements: Vec::new(),
            plan_steps: Vec::new(),
            implementation_notes: None,
            research_context: Vec::new(),
            proof: None,
            feedback: None,
        }
    }
}
