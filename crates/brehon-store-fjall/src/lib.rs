//! Fjall-based implementation of the EventStore trait.
//!
//! This crate provides a durable, ACID-compliant event store using fjall,
//! a Rust-native LSM-tree embedded database.
//!
//! # Architecture
//!
//! The store is organized into several components:
//!
//! - **keys**: Key schema for lexicographically sortable keys
//! - **store**: Main `FjallEventStore` struct implementing `EventStore`
//! - **queries**: Filter-to-index-range translation for efficient queries
//! - **views**: Materialized view persistence for task/review state
//! - **queue**: Priority-lane queue with atomic claim semantics
//! - **run_store**: Durable run and claim records
//! - **proof_store**: Durable proof bundle projection
//! - **recovery**: Startup scan for detecting incomplete states
//! - **migrations**: Schema versioning for future upgrades
//!
//! # Key Schema
//!
//! All keys are designed for efficient range scans:
//!
//! - `log:{seq}` - Global append-only event log
//! - `index:agent:{agent_id}:{seq}` - Per-agent event index
//! - `index:task:{task_id}:{seq}` - Per-task event index
//! - `index:review:{review_id}:{seq}` - Per-review event index
//! - `view:task:{task_id}` - Materialized task state
//! - `view:review:{review_id}` - Materialized review state
//! - `queue:review:{lane}:{seq}` - Priority lane with FIFO ordering
//! - `lease:review:{claim_id}` - Durable review claim/lease metadata
//! - `run:{run_id}` - Durable run record
//! - `index:run:*` - Run task/session/owner/active indexes
//! - `proof:bundle:{proof_bundle_id}` - Durable proof bundle projection
//! - `index:proof:*` - Proof task/run indexes
//!
//! # Guarantees
//!
//! - Events are atomic: write either fully commits or doesn't
//! - Global EventId/seq ordering preserved across concurrent writers
//! - Atomic claim + durable lease semantics prevent double-claiming
//! - Recovery scan detects orphaned tasks, prepared merges, expired leases
//! - Database opens cleanly after unclean shutdown

mod error;
pub mod keys;
pub mod migrations;
mod owner_lock;
pub mod proof_store;
pub mod queries;
pub mod queue;
pub mod recovery;
pub mod run_store;
pub mod store;
pub mod views;

pub use keys::{log_key, parse_seq_from_log_key, queue_key, task_view_key};
pub use proof_store::ProofStoreManager;
pub use run_store::RunStoreManager;
pub use store::{FjallEventStore, StoreError};
