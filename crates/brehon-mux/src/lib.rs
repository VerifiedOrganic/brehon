//! Brehon Terminal Multiplexer
//!
//! A terminal multiplexer built on ghostty_vt + ratatui for Brehon factory mode.
//! Provides direct PTY control for reliable prompt injection into Claude instances.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                      Multiplexer                         │
//! │  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌───────────────┐ │
//! │  │  Pane   │ │  Pane   │ │  Pane   │ │     Pane      │ │
//! │  │ worker1 │ │ worker2 │ │  super  │ │   director    │ │
//! │  │  (PTY)  │ │  (PTY)  │ │  (PTY)  │ │   (native)    │ │
//! │  └─────────┘ └─────────┘ └─────────┘ └───────────────┘ │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Each pane with a PTY has:
//! - A ghostty_vt Terminal for parsing and state management
//! - A direct write handle for prompt injection
//! - An associated agent name for targeting
//!
//! # Components
//!
//! - **ghostty_vt**: Handles terminal emulation (escape sequences, cursor, colors)
//! - **portable-pty**: Manages PTY processes
//! - **ratatui**: Renders the TUI output

pub mod agent_config;
mod error;
mod harness;
mod mux;
mod pane;
mod prompt_queue;
mod pty;
mod render;
mod session_scoped_queue;
pub mod teams;

pub use agent_config::AgentRegistry;
pub use error::{Error, Result};
pub use harness::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane, HarnessTransport,
    PromptInjectionStrategy, SupervisorCli, builtin_cli_from_launcher_shape,
};
pub use mux::{
    AgentPaneMaterialization, AsyncGatewayPromptDeliveryError, AsyncGatewayPromptDispatch, Mux,
    MuxConfig, MuxEvent, MuxRuntimeCommandPort, MuxRuntimeCommandReceiver, PaneBackendOwnership,
    PromptDeliveryAttempt, PromptQueuePosition, QuarantineOutcome,
    TerminalHostAgentFactoryBlockedPane, TerminalHostAgentFactoryPlan,
    suppress_pending_agent_health_marker_writes,
};
pub use pane::TerminalSnapshot;
pub use pane::{
    ActiveToolCall, ActivityBuffer, ActivityEntry, ActivityKind, AgentTerminalLaunchPlan,
    AgentTerminalLaunchSpec, CellStyle, DeathReason, Generation, Pane, PaneBackend, PaneId,
    PaneKind, PaneState, ReviewContextSnapshot, Rgb, TaskBlockedReason, TaskContextDetails,
    TaskContextSnapshot,
};
pub use prompt_queue::PromptQueueEntry;
pub use pty::{Pty, PtyConfig, PtyEvent, TeamsSpawnConfig};
pub use render::{LayoutDirection, Renderer};
pub use session_scoped_queue::{EntryId, ScopedEntry, SessionScopedQueue, StoredScopedEntry};
