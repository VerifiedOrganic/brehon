// The `task` tool's input_schema is expanded via `serde_json::json!` and
// brushes against the default macro recursion limit as it grows. Bump it
// here so adding a parameter does not turn into a cryptic macro error.
#![recursion_limit = "256"]
// Many tests in this crate hold a `std::sync::Mutex` env-serialization guard
// across `.await` to prevent parallel tests from racing on env vars. The lock
// is test-only and short-lived; tokio's async-aware Mutex isn't a fit because
// it's also used from sync helpers.
#![allow(clippy::await_holding_lock)]
// MCP tool handlers accept many state/context refs; bundling them into context
// structs is a separate refactor.
#![allow(clippy::too_many_arguments)]

//! MCP (Model Context Protocol) server for Brehon.
//!
//! This crate implements an MCP server that exposes tools for agents to query
//! memories, rules, skills, and task context. Agents pull what they need via
//! MCP tools instead of bulk prompt injection.
//!
//! # Architecture
//!
//! - **server**: MCP protocol handler (JSON-RPC over stdio)
//! - **tools/memory**: Memory search and management tools
//! - **tools/rules**: Project coding convention tools
//! - **tools/skills**: Reusable pattern/template tools
//! - **tools/tasks**: Task context and listing tools
//! - **session_attach**: ACP session attachment helpers

pub(crate) mod builtins;
pub(crate) mod error;
pub(crate) mod git_exec;
pub mod server;
pub mod session_attach;
pub mod tools;

pub use error::McpError;
pub use server::McpServer;
pub use session_attach::SessionAttachment;
