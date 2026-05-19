//! Runtime side-channel protocol exports.
//!
//! The canonical runtime event and command structs live in `brehon-types` so
//! core ports can depend on them without pulling in WebSocket transport code.
//! This module re-exports them from `brehon-protocol` for callers that treat
//! protocol as the public wire-contract crate.

pub use brehon_types::runtime::*;
