use crate::pane::{
    ActivityEntry, AgentTerminalLaunchSpec, DeathReason, Generation, PaneId, ReviewContextSnapshot,
    TaskContextSnapshot,
};
use brehon_types::PromptId;
use std::fmt;
use tokio::task::JoinHandle;

/// Runtime owner for a pane's terminal/process surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneBackendOwnership {
    /// Mux-owned PTY rendered through Brehon's existing ghostty_vt surface.
    GhosttyVt,
    /// Mux-owned PTY and terminal surface managed by Panesmith.
    Panesmith,
    /// Structured agent gateway session with no local PTY surface owner.
    Gateway,
    /// Terminal-host-owned pane outside the mux PTY/surface path.
    HostOwned,
    /// Brehon-native pane with no child terminal backend.
    None,
}

impl PaneBackendOwnership {
    pub fn label(self) -> &'static str {
        match self {
            Self::GhosttyVt => "ghostty_vt",
            Self::Panesmith => "panesmith",
            Self::Gateway => "gateway",
            Self::HostOwned => "host-owned",
            Self::None => "none",
        }
    }
}

/// Events from the multiplexer
#[derive(Debug, Clone)]
pub enum MuxEvent {
    /// A pane received output (includes raw bytes for client-side rendering)
    PaneOutput {
        pane_id: PaneId,
        /// Raw PTY bytes for client-side terminal emulation
        data: Vec<u8>,
        /// ACP/PTy session generation observed when output was emitted.
        generation: Generation,
    },
    /// A pane's process exited
    PaneExited {
        pane_id: PaneId,
        exit_code: Option<i32>,
    },
    /// Focus changed
    FocusChanged { from: Option<PaneId>, to: PaneId },
    /// A pane was added
    PaneAdded { pane_id: PaneId },
    /// A pane was removed
    PaneRemoved { pane_id: PaneId },
    /// Structured activity event for gateway-backed panes
    ActivityEvent {
        pane_id: PaneId,
        entry: ActivityEntry,
        generation: Generation,
    },
    /// Completion signal for a background gateway prompt delivery.
    AsyncGatewayPromptDeliveryCompleted {
        pane_id: PaneId,
        prompt: String,
        from: Option<String>,
        generation: Generation,
        result: std::result::Result<PromptDeliveryAttempt, AsyncGatewayPromptDeliveryError>,
    },
    /// Completion signal for a background Teams inbox prompt delivery.
    AsyncTeamsPromptDeliveryCompleted {
        pane_id: PaneId,
        team: String,
        generation: Generation,
        result: std::result::Result<PromptDeliveryAttempt, AsyncGatewayPromptDeliveryError>,
    },
    /// Flush pending activity output for a pane (stream end signal)
    ActivityFlush {
        pane_id: PaneId,
        generation: Generation,
    },
    /// Task context snapshot updated for a worker pane
    TaskContextChanged {
        pane_id: PaneId,
        context: Option<TaskContextSnapshot>,
    },
    /// Review context snapshot updated for a reviewer pane
    ReviewContextChanged {
        pane_id: PaneId,
        context: Option<ReviewContextSnapshot>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalHostAgentFactoryBlockedPane {
    pub pane_id: PaneId,
    pub kind: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalHostAgentFactoryPlan {
    pub total_panes: usize,
    pub launch_specs: Vec<AgentTerminalLaunchSpec>,
    pub blocked_panes: Vec<TerminalHostAgentFactoryBlockedPane>,
}

impl TerminalHostAgentFactoryPlan {
    pub fn ready(&self) -> bool {
        self.total_panes == self.launch_specs.len() && self.blocked_panes.is_empty()
    }
}

/// Outcome of a single prompt delivery attempt.
///
/// Durable queue consumers should keep the prompt on disk until
/// `Delivered { .. }` and treat `AlreadyPresent { .. }` as idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptQueuePosition {
    InFlight,
    Waiting(usize),
}

impl PromptQueuePosition {
    pub fn retry_ahead_of(self) -> usize {
        match self {
            Self::InFlight => 1,
            Self::Waiting(waiting_index) => waiting_index.saturating_add(2),
        }
    }
}

impl fmt::Display for PromptQueuePosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InFlight => write!(f, "in_flight"),
            Self::Waiting(position) => write!(f, "waiting({position})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDeliveryAttempt {
    Delivered {
        prompt_id: PromptId,
        generation: Generation,
    },
    Queued {
        prompt_id: PromptId,
        ahead_of: usize,
    },
    Rejected {
        reason: DeathReason,
    },
    AlreadyPresent {
        prompt_id: PromptId,
        position: PromptQueuePosition,
    },
}

/// Outcome of an authoritative pane quarantine request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineOutcome {
    /// The pane's effective terminal death reason after the operation.
    pub new_reason: DeathReason,
    /// Whether the pane was already in `PaneState::Dead`.
    pub was_already_dead: bool,
    /// Previous death reason when the pane had already been dead.
    pub prior_reason: Option<DeathReason>,
}

/// Async gateway prompt dispatch prepared by the mux.
///
/// The caller owns awaiting the returned task, but mux remains the source of
/// truth for gateway session bootstrap and retry eligibility.
#[derive(Debug)]
pub enum AsyncGatewayPromptDispatch {
    Started(
        JoinHandle<std::result::Result<PromptDeliveryAttempt, AsyncGatewayPromptDeliveryError>>,
    ),
    Queued {
        prompt_id: PromptId,
        ahead_of: usize,
    },
}

/// A prompt queued for delayed delivery until the transport is safe to use.
///
/// Live delayed-prompt state lives on `Pane` (see
/// `pane::enqueue_delayed_prompt`); the mux-level `pending_delayed_prompts`
/// vec is kept empty and cleared on quarantine/recycle so the surrounding
/// plumbing stays symmetric if we reintroduce a cross-pane staging queue.
#[allow(dead_code)]
pub(super) struct PendingDelayedPrompt {
    pub(super) pane_id: String,
    pub(super) prompt: String,
    pub(super) from: Option<String>,
    pub(super) inject_after: std::time::Instant,
    pub(super) generation: Generation,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RecycleMarker {
    pub(super) at: std::time::Instant,
    pub(super) generation: Generation,
}
/// Default delay before delivering startup prompts (allows CLIs to initialize).
pub(super) const PTY_STARTUP_PROMPT_DELAY_SECS: u64 = 5;
/// Additional stagger between queued startup prompts to avoid a startup herd.
pub(super) const STARTUP_PROMPT_STAGGER_MILLIS: u64 = 400;
/// Default maximum queued events to drain from the mux channel per UI poll.
///
/// This prevents chatty gateway sessions from starving the TUI render loop.
pub(super) const DEFAULT_MAX_QUEUED_EVENTS_PER_POLL: usize = 256;
/// Hard cap for queued events drained in one UI poll.
pub(super) const MAX_QUEUED_EVENTS_PER_POLL: usize = 512;
/// Maximum PTY/output bytes to process in one mux batch across all panes.
///
/// Pane-level drains stay individually bounded; this global cap prevents the
/// sum of many active panes from turning one TUI tick into a large terminal
/// parsing/rendering batch. Leftover bytes remain queued for later ticks.
pub(super) const MAX_OUTPUT_BYTES_PER_POLL: usize = 256 * 1024;
/// Default event channel capacity.
pub(super) const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 256;
/// Events per worker to add to the channel capacity and poll limit.
pub(super) const EVENTS_PER_WORKER: usize = 128;
/// Maximum delayed prompts synchronously dispatched from one state-machine tick.
pub(super) const MAX_READY_PROMPT_DISPATCHES_PER_TICK: usize = 2;
/// Minimum quiet time before sending a Claude Teams nudge.
pub(super) const TEAMS_NUDGE_QUIET_THRESHOLD: std::time::Duration =
    std::time::Duration::from_millis(800);
/// Minimum quiet time before auto-injecting the next PTY prompt into Ink-based CLIs.
pub(super) const PTY_INK_PROMPT_QUIET_THRESHOLD: std::time::Duration =
    std::time::Duration::from_millis(800);
/// Delay before retrying a queued ACP gateway prompt while the current turn is still active.
pub(super) const GATEWAY_PROMPT_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
/// Ground-truth quiet window for Busy → Ready state transitions.
pub(crate) const QUIET_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(30);
/// Absolute upper bound on a single in-flight turn before forced Busy → Ready.
pub(crate) const MAX_TURN_DURATION: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// Maximum time a supervisor Teams inbox message may remain queued before we
/// force control-only recovery to trigger a turn.
pub(super) const SUPERVISOR_INBOX_ESCALATION_DELAY: std::time::Duration =
    std::time::Duration::from_secs(15);
/// Minimum output-quiet time before we forcibly interrupt a stuck supervisor inbox nudge.
pub(super) const SUPERVISOR_INBOX_ESCALATION_QUIET_THRESHOLD: std::time::Duration =
    std::time::Duration::from_secs(5);
/// Cooldown applied to the supervisor pane after each forced inbox-recovery
/// attempt. This prevents the recovery loop from re-firing every tick when a
/// stale draft cannot be submitted in a single pass (e.g. Ctrl-C must clear
/// the draft on tick N before an Enter nudge runs on tick N+1). Long enough
/// for Claude Code to redraw its prompt after Ctrl-C; short enough that real
/// inbox events aren't perceptibly delayed.
pub(super) const SUPERVISOR_INBOX_RECOVERY_COOLDOWN: std::time::Duration =
    std::time::Duration::from_millis(2_500);

#[derive(Debug, Clone)]
pub struct AsyncGatewayPromptDeliveryError {
    pub error: String,
}
