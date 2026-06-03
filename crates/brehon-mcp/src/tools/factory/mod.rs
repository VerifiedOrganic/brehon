//! Factory orchestration tool for MCP.
//!
//! Action-based tool for worker management, status, and assignment.
//! `assign_workers` persists assignments to the same task JSON files
//! that `task_actions` reads, so workers see tasks via `task mine`.

mod git_sync;
mod paths;
mod tool;
mod workers;
mod worktree_ops;

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;

// --- Public re-exports ---

pub use tool::FactoryTool;
