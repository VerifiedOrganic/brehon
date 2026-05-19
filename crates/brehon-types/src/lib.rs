//! Brehon core types - domain entities, events, and configuration structures.
//!
//! This crate contains all shared types used across the Brehon system. It has zero
//! external dependencies beyond serde, chrono, uuid, and thiserror.

pub mod agent;
pub mod budget;
pub mod config;
#[cfg(test)]
mod config_tests;
pub mod context;
pub mod decision;
pub mod drain;
pub mod event;
pub mod event_index;
pub mod feedback;
pub mod message;
pub mod proof;
pub mod review;
pub mod role;
pub mod role_protocol;
pub mod run;
pub mod runtime;
pub mod stability;
pub mod system;
pub mod task;
pub mod view;
pub mod worker_protocol;

pub use agent::*;
pub use budget::*;
pub use config::*;
pub use context::*;
pub use decision::*;
pub use drain::*;
pub use event::*;
pub use feedback::*;
pub use message::*;
pub use proof::*;
pub use review::*;
pub use role::*;
pub use role_protocol::*;
pub use run::*;
pub use runtime::*;
pub use stability::*;
pub use system::*;
pub use task::*;
pub use view::*;
pub use worker_protocol::*;
