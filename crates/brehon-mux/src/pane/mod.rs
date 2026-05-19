//! Pane abstraction using ghostty_vt for terminal emulation
//!
//! A pane combines:
//! - A PTY process (optional - director pane is native)
//! - A ghostty_vt Terminal for state management
//! - Metadata (agent name, role, etc.)

pub mod activity;
mod host_launch;
mod pty_io;
mod snapshot;
pub(crate) mod spawn;
mod state;
mod style;
pub(crate) mod terminal;
#[cfg(test)]
mod tests;
mod types;

pub use activity::{ActiveToolCall, ActivityBuffer, ActivityEntry, ActivityKind};
pub use brehon_protocol::TerminalSnapshot;
// Re-export ghostty_vt cell/style types so downstream renderers don't have
// to take a direct dep on ghostty_vt just to read pane row styles.
pub use ghostty_vt::{CellStyle, Rgb};
pub use host_launch::{AgentTerminalLaunchPlan, AgentTerminalLaunchSpec};
pub(crate) use state::{DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP, QueuedPrompt};
pub use state::{DeathReason, Generation, PaneState};
pub(crate) use types::GatewaySpawnConfig;
#[allow(unused_imports)] // Used by test module
pub(crate) use types::{BufferedMessage, ClaudePromptState, InjectionMode};
pub use types::{
    Pane, PaneBackend, PaneId, PaneKind, ReviewContextSnapshot, TaskBlockedReason,
    TaskContextDetails, TaskContextSnapshot,
};
