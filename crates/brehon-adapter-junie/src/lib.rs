//! Junie CLI adapter for Brehon.
//!
//! This crate implements the [`AgentAdapter`](brehon_adapter_sdk::AgentAdapter) trait for the JetBrains Junie
//! CLI, enabling Brehon to spawn Junie sessions, send prompts, and receive
//! events through the standard adapter SDK interface. It also exports
//! [`JunieSessionConfig`] and [`JunieSpawnParams`] so that `brehon-pty` can
//! continue to build PTY spawn configurations without duplicating Junie-
//! specific logic.

pub mod junie;

pub use junie::{
    JunieAdapter, JunieConfig, JunieError, JunieSession, JunieSessionConfig, JunieSpawnParams,
};
