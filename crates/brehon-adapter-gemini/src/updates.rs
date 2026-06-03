//! Session event types — re-exported from [`brehon_adapter_sdk`].
//!
//! The canonical definitions live in `brehon_adapter_sdk::session_event`.
//! This module re-exports them for backward compatibility.

// Re-export all session event types and functions from the adapter SDK.
pub use brehon_adapter_sdk::session_event::{
    normalize_session_update_value, session_event_to_domain_event, SessionEvent, UpdateError,
};
