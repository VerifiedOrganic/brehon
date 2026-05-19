//! Test harness for the Brehon system.
//!
//! This crate provides mock/stub implementations of all port traits for
//! deterministic testing. Every subsequent phase uses this harness.
//!
//! # Features
//!
//! - **Mock implementations**: In-memory implementations of all port traits
//! - **Scripted responses**: Pre-configured response sequences for agents
//! - **Chaos testing**: Randomized failures, delays, and duplications
//! - **Scenario parsing**: YAML-based scenario definitions
//! - **Crash injection**: Framework for crash recovery testing
//! - **Stress testing**: Concurrent load generators
//! - **Fixtures**: Pre-built behavior configurations for common scenarios

pub mod assertions;
pub mod chaos;
pub mod crash_injector;
pub mod fixtures;
pub mod mock_agent;
pub mod mock_decision;
pub mod mock_gateway;
pub mod mock_git;
pub mod mock_mcp;
pub mod mock_notifications;
pub mod mock_run_store;
pub mod mock_store;
pub mod run_store_contract;
pub mod scenario;
pub mod stress;

pub use assertions::*;
pub use chaos::ChaosConfig;
pub use chaos::ChaosInjector;
pub use crash_injector::{CrashInjector, CrashPoint, CrashScenario, SubprocessHandle};
pub use fixtures::*;
pub use mock_agent::MockBehavior;
pub use mock_decision::MockDecisionEngine;
pub use mock_gateway::MockGateway;
pub use mock_git::FakeGitOperations;
pub use mock_mcp::{
    MockEchoTool, MockMcpError, MockMcpServer, MockPanicTool, MockSlowTool, MockTool,
    MockToolResult,
};
pub use mock_notifications::RecordingNotificationSink;
pub use mock_run_store::InMemoryRunStore;
pub use mock_store::InMemoryEventStore;
pub use scenario::{Scenario, ScenarioRunner};
