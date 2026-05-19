//! Brehon Doctor — diagnostic checks and findings for worktrees, runtime, tasks, and reviews.
//!
//! This crate provides:
//! - Diagnostic checkers that examine system state for issues
//! - Report formatting with category and severity grouping
//! - Entry point to run all checks and produce reports

pub mod checkers;
pub mod doctor;
pub mod report;
pub mod types;

pub use doctor::{run_doctor, run_doctor_cli, run_doctor_compact, run_doctor_json};
pub use types::*;
