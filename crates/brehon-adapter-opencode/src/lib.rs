//! OpenCode adapter for Brehon.
//!
//! Provides [`OpenCodeServerSession`] for managing OpenCode agent sessions
//! over HTTP, plus an [`brehon_adapter_sdk::AgentAdapter`] implementation.

pub mod opencode;
pub mod process;

pub use opencode::OpenCodeServerSession;
