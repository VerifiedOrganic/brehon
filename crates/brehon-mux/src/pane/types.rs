//! Type definitions for the pane module.

use brehon_acp::GatewayProtocol;
use brehon_pty::PtyConfig;
use brehon_types::task::{Task, TaskStatus};
use std::path::PathBuf;
use std::time::Instant;

use crate::pane::PaneState;
use crate::pane::activity::ActivityBuffer;
use crate::pane::state::{Generation, PanePromptQueue};

/// Unique identifier for a pane
pub type PaneId = String;

/// Snapshot of task context for a worker pane.
///
/// Provides durable task metadata from Brehon runtime state (runtime/tasks/*.json).
/// Self-contained with no references to store or event stream.
/// Reason a task is blocked, with optional blocker details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBlockedReason {
    /// Blocking task identifier when known (e.g., "T-xyz123").
    pub blocker_task_id: Option<String>,
    /// Human summary from task store state.
    pub summary: Option<String>,
}

/// Extended task metadata used to build a `TaskContextSnapshot`.
#[derive(Debug, Clone, Default)]
pub struct TaskContextDetails {
    /// Completion mode (merge/close)
    pub completion_mode: Option<String>,
    /// Target branch for merge tasks
    pub merge_target: Option<String>,
    /// Parent epic ID if this is a subtask
    pub parent_id: Option<String>,
    /// Epic branch for feature epics
    pub epic_branch: Option<String>,
    /// Epic integration worktree path for feature epics
    pub epic_worktree: Option<PathBuf>,
    /// Structured blocker reason sourced from runtime state
    pub blocked_reason: Option<TaskBlockedReason>,
}

/// Snapshot of task context for a worker pane, built from runtime task state.
#[derive(Debug, Clone)]
pub struct TaskContextSnapshot {
    /// Task ID (e.g., "T-abc123")
    pub task_id: String,
    /// Task title
    pub title: String,
    /// Current status
    pub status: TaskStatus,
    /// Completion mode (merge/close)
    pub completion_mode: Option<String>,
    /// Target branch for merge tasks
    pub merge_target: Option<String>,
    /// Parent epic ID if this is a subtask
    pub parent_id: Option<String>,
    /// Epic branch for feature epics
    pub epic_branch: Option<String>,
    /// Epic integration worktree path for feature epics
    pub epic_worktree: Option<PathBuf>,
    /// Structured blocker reason if blocked
    pub blocked_reason: Option<TaskBlockedReason>,
    /// When this snapshot was last updated
    pub updated_at: Instant,
}

impl TaskContextSnapshot {
    /// Build a snapshot from a runtime `Task` and extended details.
    pub fn from_task(task: &Task, details: TaskContextDetails) -> Self {
        Self {
            task_id: task.id.as_str().to_string(),
            title: task.title.clone(),
            status: task.status,
            completion_mode: details.completion_mode,
            merge_target: details.merge_target,
            parent_id: details.parent_id,
            epic_branch: details.epic_branch,
            epic_worktree: details.epic_worktree,
            blocked_reason: details.blocked_reason,
            updated_at: Instant::now(),
        }
    }

    /// Check if this task is in a terminal state (merged/closed).
    pub fn is_terminal(&self) -> bool {
        matches!(self.status, TaskStatus::Merged)
    }
}

/// Snapshot of review context for a reviewer pane.
///
/// Built from in-memory review coordinator state and emitted on review state
/// transitions.
#[derive(Debug, Clone)]
pub struct ReviewContextSnapshot {
    /// Review ID (e.g., "R-abc123")
    pub review_id: String,
    /// Task ID under review
    pub task_id: String,
    /// Current round number
    pub round: u32,
    /// Number of reviewers in the panel
    pub panel_total: usize,
    /// Number of reviewers complete in current round
    pub panel_done: usize,
    /// Finalized verdict when available (e.g. "approve", "changes_requested", "reject", "escalated")
    pub verdict: Option<String>,
    /// Finalized score (rounded aggregate) when available
    pub score: Option<u8>,
    /// Concise findings summary when available
    pub findings_summary: Option<String>,
    /// When this snapshot was last updated
    pub updated_at: Instant,
}

/// The kind of pane
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneKind {
    /// Worker agent (Claude/Codex CLI)
    Worker,
    /// Supervisor agent (Claude/Codex CLI)
    Supervisor,
    /// Director (native TUI, no PTY)
    Director,
    /// Generic shell
    Shell,
    /// Reviewer agent (code review behind review authority)
    Reviewer,
    /// Advisor agent (read-only brainstorming/chat)
    Advisor,
    /// Research agent (read-only context/artifact producer)
    Research,
}

impl PaneKind {
    /// Return the string representation of this pane kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Supervisor => "supervisor",
            Self::Director => "director",
            Self::Shell => "shell",
            Self::Reviewer => "reviewer",
            Self::Advisor => "advisor",
            Self::Research => "research",
        }
    }
}

/// Backend for a pane — either a PTY (Claude/Codex interactive) or
/// none (director pane).
pub enum PaneBackend {
    /// No backend (director pane — rendered natively)
    None,
    /// PTY-based interactive terminal (Claude, Codex)
    Pty(crate::pty::Pty),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // WIP: used by tests, not yet wired into lib paths
pub(crate) enum InjectionMode {
    Immediate,
    Buffered,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // WIP: fields used by tests and upcoming injection logic
pub(crate) struct BufferedMessage {
    pub(crate) prompt_id: i64,
    pub(crate) source: String,
    pub(crate) queue_target: String,
    pub(crate) prompt: String,
    pub(crate) summary: Option<String>,
    pub(crate) priority: i32,
    pub(crate) enqueued_at: Instant,
    pub(crate) mode: InjectionMode,
}

/// Spawn configuration for gateway-managed agents.
/// Built from PtyConfig but used with AgentProcess (piped stdio, no PTY).
#[derive(Debug, Clone)]
pub(crate) struct GatewaySpawnConfig {
    pub(crate) command: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) cwd: String,
    pub(crate) protocol: GatewayProtocol,
    pub(crate) tool_prefix: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key_env: Option<String>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) model: Option<String>,
    pub(crate) sidecar_socket_path: Option<String>,
    pub(crate) sidecar_ready_path: Option<String>,
    pub(crate) sidecar_connect_timeout_ms: Option<u64>,
}

/// A pane in the multiplexer
pub struct Pane {
    /// Unique identifier (usually agent name)
    pub(crate) id: PaneId,
    /// What kind of pane
    pub(crate) kind: PaneKind,
    /// The ghostty_vt terminal (handles escape sequences, cursor, colors)
    pub(crate) terminal: ghostty_vt::Terminal,
    /// Process backend
    pub(crate) backend: PaneBackend,
    /// Whether this pane has focus
    pub(crate) focused: bool,
    /// Title for display
    pub(crate) title: String,
    /// Color for the pane border (hex)
    pub(crate) color: Option<String>,
    /// Whether the process has exited
    pub(crate) exited: bool,
    /// Exit code if exited
    pub(crate) exit_code: Option<i32>,
    /// Terminal dimensions
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// Optional recording writer for session capture
    pub(crate) recorder:
        Option<std::sync::Arc<tokio::sync::Mutex<brehon_recording::RecordingWriter>>>,
    /// Whether to force all rows dirty on next take (for new client sync)
    pub(crate) force_all_dirty: bool,
    /// Monotonic counter that advances whenever this pane's visible state
    /// could have changed (parser fed, resize, scroll, synthetic append,
    /// teardown rebuild). Used by the TUI render widget to skip the
    /// per-row FFI round-trip when nothing has changed since the last
    /// frame — distinct from `take_dirty_rows`, which the WebSocket
    /// snapshot path consumes destructively.
    pub(crate) render_generation: u64,
    /// Last known total scrollback lines (for scroll detection)
    pub(crate) last_total_scrollback: u32,
    /// Sequence counter for incremental updates (pane-scoped)
    pub(crate) seq_counter: u64,
    /// Agent adapter running in this pane (affects key sequences for prompt injection)
    pub(crate) cli_type: crate::harness::AgentAdapter,
    /// Configured agent alias for this pane when different from the built-in CLI
    /// family (for example `claude-reviewer` vs `claude`).
    pub(crate) configured_agent_type: Option<String>,
    /// Last time PTY output was observed for this pane
    pub(crate) last_output_at: Instant,
    /// Whether the daemon considers a tool to still be executing in this pane
    pub(crate) is_tool_executing: bool,
    /// Queued PTY injections waiting for a safe idle window
    pub(crate) pending_messages: std::collections::VecDeque<BufferedMessage>,
    /// Best-effort cleanup path for per-agent notification sockets
    pub(crate) notify_socket_path: Option<PathBuf>,
    /// Stable BREHON_SESSION_ID assigned at spawn time, if this pane launches
    /// an Brehon-aware agent process.
    pub(crate) agent_session_id: Option<String>,
    /// Text waiting for echo confirmation before submitting (Ink-based CLIs).
    /// Contains (needle text to detect, deadline for fallback, generation).
    /// Behind Arc<std::sync::Mutex> so inject_prompt can share it with a
    /// spawned tokio fallback timer that guarantees Enter delivery.
    /// The generation counter prevents stale timers from sending Enter for
    /// a newer injection.
    pub(crate) pending_ink_submit: std::sync::Arc<std::sync::Mutex<Option<(String, Instant, u64)>>>,
    /// Monotonic counter for ink echo injection generations.
    pub(crate) ink_submit_generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Tracks whether the last synthetic output byte was '\r' so lone '\n'
    /// from non-PTY transports can be normalized to CRLF across chunk boundaries.
    pub(crate) synthetic_prev_was_cr: bool,
    /// Carries incomplete supervisor startup MCP blocks across PTY chunks so
    /// they can be suppressed only once the full block boundary is known.
    pub(crate) supervisor_pending_structured_output: Vec<u8>,
    /// Gateway session ID for structured-transport agents.
    /// When set, this pane's agent runs via `AcpGateway` with piped stdio
    /// instead of PTY. Prompts are delivered through `gateway.send_prompt()`.
    pub(crate) gateway_session_id: Option<String>,
    /// Monotonic ACP session generation for this pane.
    ///
    /// Starts at `Generation(0)` and increments each time a new ACP session
    /// is spawned for this pane.
    pub(crate) current_generation: Generation,
    /// Spawn config for PTY-backed agents that can be restarted in place.
    pub(crate) pty_spawn_config: Option<PtyConfig>,
    /// Spawn config for gateway agents.
    /// Stored at pane creation for structured agents, consumed when the gateway
    /// session is initialized.
    pub(crate) gateway_spawn_config: Option<GatewaySpawnConfig>,
    /// Attached gateway terminal ID for manual keyboard input.
    pub(crate) gateway_terminal_id: Option<String>,
    /// Tracks whether the gateway session event bridge has been registered.
    pub(crate) gateway_event_bridge_started: bool,
    /// Whether a Claude Teams inbox delivery still needs a turn-trigger nudge.
    pub(crate) pending_inbox_nudge: bool,
    /// When the current pending Claude Teams inbox nudge was first queued.
    pub(crate) pending_inbox_nudge_since: Option<Instant>,
    /// Earliest time a Claude Teams inbox nudge may be sent after spawn/reset.
    pub(crate) inbox_nudge_not_before: Option<Instant>,
    /// Activity buffer for gateway-backed panes.
    /// Stores structured activity entries with bounded retention.
    pub(crate) activity_buffer: Option<ActivityBuffer>,
    /// Per-pane delayed prompt queue (single in-flight + bounded FIFO waitlist).
    pub(crate) prompt_queue: PanePromptQueue,
    /// Authoritative pane lifecycle state.
    pub(crate) pane_state: Option<PaneState>,
    /// Task context snapshot for worker panes.
    /// Populated from runtime/tasks/*.json when a task is assigned.
    pub(crate) task_context: Option<TaskContextSnapshot>,
    /// Review context snapshot for reviewer panes.
    pub(crate) review_context: Option<ReviewContextSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudePromptState {
    None,
    Visible,
    Empty,
    Draft,
}
