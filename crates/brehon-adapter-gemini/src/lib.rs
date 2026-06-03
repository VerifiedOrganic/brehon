//! Gemini CLI adapter for Brehon.
//!
//! This crate implements the [`AgentAdapter`](brehon_adapter_sdk::AgentAdapter) trait for the Google Gemini CLI,
//! enabling Brehon to spawn Gemini sessions, send prompts, and receive events
//! through the standard adapter SDK interface.

pub mod acp_types;
pub mod process;
pub mod protocol;
pub mod stability_runtime;
pub mod updates;

pub mod gemini;

pub use gemini::{GeminiAdapter, GeminiConfig, GeminiError, GeminiSession};
