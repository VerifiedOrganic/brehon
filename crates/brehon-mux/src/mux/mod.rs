//! Main multiplexer
//!
//! Manages multiple panes, handles input routing, and coordinates rendering.

use crate::harness::{AgentAdapter, SupervisorCli};
#[cfg(test)]
use crate::pane::PaneKind;
use crate::pane::panesmith_shim::BrehonPanesmithShim;
use crate::pane::{Pane, PaneId};
use crate::teams::TeamsManager;
use brehon_ports::{PolicyGate, RuntimeEventSink};
use indexmap::IndexMap;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

mod activity;
mod backend;
mod command;
mod config;
mod dispatch;
mod events;
mod format;
mod lifecycle;
mod panesmith;
mod policy;
mod poll_budget;
mod runtime;
mod stability;
mod supervisor_recovery;
mod types;

pub use command::{MuxRuntimeCommandPort, MuxRuntimeCommandReceiver};
pub use config::{AgentPaneMaterialization, MuxConfig};
pub use stability::suppress_pending_agent_health_marker_writes;
pub use types::{
    AsyncGatewayPromptDeliveryError, AsyncGatewayPromptDispatch, MuxEvent, PaneBackendOwnership,
    PromptDeliveryAttempt, PromptQueuePosition, QuarantineOutcome,
    TerminalHostAgentFactoryBlockedPane, TerminalHostAgentFactoryPlan,
};
pub(crate) use types::{MAX_TURN_DURATION, QUIET_THRESHOLD};

#[cfg(test)]
pub(crate) use format::{
    format_acp_session_event, normalize_gateway_tool_event, prompt_delivery_notice,
    session_event_to_activity_entry,
};

/// The main terminal multiplexer
pub struct Mux {
    /// All panes, keyed by ID (insertion order preserved)
    panes: IndexMap<PaneId, Pane>,
    /// Panesmith manager and id registry for supervisor PTY dogfood panes.
    panesmith: BrehonPanesmithShim,
    /// Mux events mirrored from Panesmith when callers use one-at-a-time poll().
    pending_panesmith_events: VecDeque<types::MuxEvent>,
    /// Currently focused pane
    focused: Option<PaneId>,
    /// Event sender
    event_tx: mpsc::Sender<MuxEvent>,
    /// Event receiver
    event_rx: mpsc::Receiver<MuxEvent>,
    /// Terminal size
    rows: u16,
    cols: u16,
    /// Worker adapter used for dynamic worker spawns
    worker_cli: AgentAdapter,
    /// Worker model used for dynamic worker spawns
    worker_model: Option<String>,
    /// Supervisor name (stored for building startup prompts)
    supervisor_name: String,
    /// Logical runtime session name for this mux instance.
    session_name: Option<String>,
    /// Native Agent Teams manager for Claude Code inbox delivery.
    /// When set, Claude Code agents receive prompts through inbox files
    /// rather than PTY injection.
    teams: Option<TeamsManager>,
    /// Agent gateway for structured prompt delivery.
    /// ACP-capable agents (Codex, Gemini, OpenCode) are spawned through this
    /// gateway with piped stdio. Prompts are delivered via `send_prompt()`.
    gateway: Option<brehon_acp::AcpGateway>,
    /// Prompts waiting for a delayed delivery window to open.
    pending_delayed_prompts: Vec<types::PendingDelayedPrompt>,
    /// Last authoritative recycle marker per pane for idempotent replays.
    recycle_markers: HashMap<PaneId, types::RecycleMarker>,
    /// Best-effort count of gateway-reported in-flight operations per pane.
    ///
    /// Used to suppress false idle detection for quieter structured transports
    /// like OpenCode server sessions that can be active without emitting PTY
    /// output continuously.
    active_gateway_operations: HashMap<PaneId, usize>,
    /// Shared checkout root used to validate isolated pane cwd values.
    shared_repo_root: Option<PathBuf>,
    /// Whether this mux instance expects all agent panes to run in linked worktrees.
    worktree_isolation: bool,
    /// Factory for direct API tool bridges, injected by the CLI layer.
    direct_tool_bridge_factory: Option<Arc<dyn brehon_acp::DirectToolBridgeFactory>>,
    /// Maximum queued events to drain from the mux channel per UI poll.
    max_queued_events_per_poll: usize,
    /// Next pane index to start output draining from.
    ///
    /// Rotating the start index keeps a continuously chatty early pane from
    /// starving later panes when the global per-poll output budget is hit.
    next_output_drain_index: usize,
    /// Optional side-channel sink for terminal-host agnostic runtime events.
    runtime_event_sink: Option<Arc<dyn RuntimeEventSink>>,
    /// Optional policy gate for audited mutating runtime operations.
    policy_gate: Option<Arc<dyn PolicyGate>>,
}

impl Mux {
    pub fn new(rows: u16, cols: u16) -> Self {
        let (event_tx, event_rx) = mpsc::channel(types::DEFAULT_EVENT_CHANNEL_CAPACITY);
        Self {
            panes: IndexMap::new(),
            panesmith: BrehonPanesmithShim::new(),
            pending_panesmith_events: VecDeque::new(),
            focused: None,
            event_tx,
            event_rx,
            rows,
            cols,
            worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
            worker_model: None,
            supervisor_name: "supervisor".to_string(),
            session_name: None,
            teams: None,
            gateway: None,
            pending_delayed_prompts: Vec::new(),
            recycle_markers: HashMap::new(),
            active_gateway_operations: HashMap::new(),
            shared_repo_root: None,
            worktree_isolation: false,
            direct_tool_bridge_factory: None,
            max_queued_events_per_poll: types::DEFAULT_MAX_QUEUED_EVENTS_PER_POLL,
            next_output_drain_index: 0,
            runtime_event_sink: None,
            policy_gate: None,
        }
    }
}

#[cfg(test)]
#[path = "../mux_tests/mod.rs"]
mod tests;
