//! Codex (OpenAI CLI) adapter for Brehon.
//!
//! This crate implements the [`AgentAdapter`](brehon_adapter_sdk::AgentAdapter) trait for the Codex app-server
//! websocket protocol, enabling Brehon to manage Codex sessions through a
//! structured adapter interface.

pub mod codex;

pub use codex::{CodexError, CodexWsSession, CodexWsSessionInner};
