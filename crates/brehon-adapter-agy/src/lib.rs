//! Antigravity 2.0 CLI (agy) adapter for Brehon.
//!
//! This crate implements the [`AgentAdapter`](brehon_adapter_sdk::AgentAdapter) trait for Google's
//! Antigravity 2.0 CLI (agy), enabling Brehon to spawn agy sessions, send prompts, and receive
//! events through the standard adapter SDK interface. It also exports
//! [`AgySessionConfig`] and [`AgySpawnParams`] so that `brehon-pty` can
//! continue to build PTY spawn configurations without duplicating agy-
//! specific logic.

pub mod agy;

pub use agy::{AgyAdapter, AgyConfig, AgyError, AgySession, AgySessionConfig, AgySpawnParams};
