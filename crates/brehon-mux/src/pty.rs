//! PTY management - re-exported from brehon-pty crate
//!
//! This module re-exports the brehon-pty crate for backwards compatibility.
//! The PTY implementation has been extracted to allow reuse in other crates
//! like the Tauri desktop app.

pub use brehon_pty::*;
