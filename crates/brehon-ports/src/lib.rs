//! Port traits for the Brehon system.
//!
//! This crate defines the port (trait) interfaces for all external dependencies.
//! Ports are abstract interfaces that isolate the core domain from infrastructure
//! concerns.
//!
//! # Architecture
//!
//! Following hexagonal architecture (ports and adapters):
//! - **Ports** are traits defined in this crate
//! - **Adapters** are implementations of these traits in separate crates
//!
//! # Available Ports
//!
//! - [`EventStore`] - Event sourcing persistence
//! - [`AgentGateway`] - Agent session communication
//! - [`DecisionEngine`] - AI-assisted decisions
//! - [`SearchIndex`] - Memory/rule/skill search
//! - [`GitOperations`] - Git repository operations
//! - [`NotificationSink`] - TUI notifications
//! - [`RuntimeEventSink`] - Runtime side-channel event publication
//! - [`RuntimeCommandPort`] - Mutating runtime command execution
//! - [`RuntimeCommandRouter`] - Policy-routed runtime command submission
//! - [`TerminalHostAdapter`] - Replaceable terminal host boundary
//! - [`TerminalHostEventObserver`] - Host state observation boundary
//! - [`PolicyGate`] - Mutating runtime command policy decisions
//! - [`DetectionEngine`] - Advisory semantic detection
//! - [`RunStore`] - Durable run and claim ownership
//! - [`ProofStore`] - Durable proof bundle projection
//!
//! # Error Handling
//!
//! All ports use [`PortError`] as their error type, with variants for each
//! error category (Storage, Agent, Git, Config, Notification, IO).

pub mod agent_gateway;
pub mod decision;
pub mod error;
pub mod event_store;
pub mod git_ops;
pub mod notification;
pub mod proof_store;
pub mod run_store;
pub mod runtime;
pub mod search_index;

pub use agent_gateway::AgentGateway;
pub use decision::DecisionEngine;
pub use error::PortError;
pub use event_store::EventStore;
pub use git_ops::{
    ConflictEntry, ConflictType, Diff, FileDiff, GitOperations, MergeResult,
    RebaseFallbackStrategy, RebaseResult,
};
pub use notification::{ModalAction, Notification, NotificationLevel, NotificationSink, TabId};
pub use proof_store::{ProofStore, ProofStoreError, ProofStoreResult};
pub use run_store::{
    ClaimRelease, ClaimRequest, RetryAttemptRequest, RetryAttemptStarted, RunCompletion,
    RunContinuation, RunFailure, RunRetry, RunStore, RunStoreError, RunStoreResult,
};
pub use runtime::{
    DetectionEngine, NoopRuntimeEventSink, PolicyGate, RuntimeCommandPort, RuntimeCommandRouter,
    RuntimeEventSink, RuntimeEventStream, TerminalHostAdapter, TerminalHostEventObserver,
};
pub use search_index::SearchIndex;
