//! ACP (Agent Client Protocol) implementation for Brehon.
//!
//! This crate implements the `AgentGateway` and `DecisionEngine` traits
//! from `brehon-ports` using the ACP protocol over stdio.
//!
//! # Architecture
//!
//! The crate is organized into several modules:
//!
//! - `protocol`: JSON-RPC 2.0 message types and parsing
//! - `acp_types`: ACP-specific protocol types
//! - `process`: Subprocess management for agent processes
//! - `session`: ACP session lifecycle management
//! - `lifecycle`: Session initialization and teardown
//! - `config`: Session config application
//! - `updates`: Session update normalization to domain events
//! - `permissions`: Permission mediation
//! - `terminals`: Terminal support and transcript fallback
//! - `instrument`: Auto-instrumentation for events
//! - `decision`: DecisionEngine implementation
//! - `gateway`: AgentGateway implementation (AcpGateway)
//!
//! # Usage
//!
//! ```rust,no_run
//! use brehon_acp::{AcpGateway, AcpSession};
//! use brehon_ports::AgentGateway;
//! use brehon_types::{SessionSpec, AgentId};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut gateway = AcpGateway::new();
//!     gateway.register_agent("test-agent", "/usr/bin/agent", vec![]);
//!
//!     let spec = SessionSpec {
//!         agent_id: AgentId::new("test-agent"),
//!         role: "worker".into(),
//!         worktree_path: "/tmp/work".into(),
//!         merge_target: None,
//!     };
//!
//!     let session_id = gateway.spawn(spec).await.unwrap();
//!     println!("Spawned session: {}", session_id);
//! }
//! ```

pub(crate) mod config;
pub mod decision;
pub(crate) mod direct_tools;
pub mod gateway;
pub(crate) mod instrument;
pub mod lifecycle;
pub(crate) mod peer;
pub use brehon_adapter_codex::codex;
pub use brehon_adapter_openai::openai_compatible;
pub use brehon_adapter_opencode::opencode;
pub(crate) mod permissions;
pub mod session;
pub(crate) mod terminals;

// Re-export shared infrastructure from brehon-adapter-gemini to eliminate duplication.
pub use brehon_adapter_gemini::acp_types;
pub use brehon_adapter_gemini::gemini;
pub use brehon_adapter_gemini::process;
pub use brehon_adapter_gemini::protocol;
pub use brehon_adapter_gemini::stability_runtime;
pub use brehon_adapter_gemini::updates;

// Re-exported from brehon-adapter-sdk for backward compatibility (session_event only;
// process, protocol, and stability_runtime are re-exported from brehon-adapter-gemini above).
pub use brehon_adapter_sdk::session_event::{
    self, normalize_session_update_value, session_event_to_domain_event, SessionEvent, UpdateError,
};

pub use decision::AcpDecisionEngine;
pub use direct_tools::{DirectToolBridge, DirectToolBridgeFactory};
pub use gateway::{
    create_gateway, create_gateway_with_config, AcpGateway, AgentLaunchConfig, GatewayProtocol,
};
pub use lifecycle::SessionConfig;
pub use session::AcpSession;
