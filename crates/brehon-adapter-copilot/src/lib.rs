//! GitHub Copilot CLI adapter for Brehon.
//!
//! This crate implements the [`AgentAdapter`] trait for the GitHub Copilot CLI,
//! enabling Brehon to spawn Copilot sessions, send prompts, and receive events
//! through the standard adapter SDK interface. Copilot communicates via ACP
//! (Agent Client Protocol) over stdio.

pub mod copilot;

pub use copilot::{
    copilot_launch_command, desired_copilot_mcp_config, prepare_local_copilot_runtime,
    prepare_local_copilot_runtime_with_global_config, CopilotAdapter, CopilotConfig, CopilotError,
    CopilotSession,
};
