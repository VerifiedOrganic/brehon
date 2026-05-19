//! Diagnostic checkers for different aspects of Brehon state.

pub mod review;
pub mod runtime;
pub mod store_search;
pub mod task;
pub mod worktree;

use crate::types::{DiagnosticCategory, DiagnosticFinding};
use std::path::Path;

/// Trait for diagnostic checkers.
pub trait Checker {
    /// Category of issues this checker finds.
    fn category(&self) -> DiagnosticCategory;

    /// Run the checker and return findings.
    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error>;
}

/// Run all diagnostic checkers and return combined findings.
pub fn run_all_checks(
    brehon_root: &Path,
    runtime_dir: &Path,
) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
    let mut findings = Vec::new();

    // Worktree checks
    let worktree_checker = worktree::WorktreeChecker::new(brehon_root);
    findings.extend(worktree_checker.check()?);

    // Runtime checks
    let runtime_checker = runtime::RuntimeChecker::new(runtime_dir);
    findings.extend(runtime_checker.check()?);

    // Task checks
    let task_checker = task::TaskChecker::new(runtime_dir);
    findings.extend(task_checker.check()?);

    // Review checks
    let review_checker = review::ReviewChecker::new(runtime_dir);
    findings.extend(review_checker.check()?);

    // Store/search checks
    let store_search_checker = store_search::StoreSearchChecker::new(brehon_root);
    findings.extend(store_search_checker.check()?);

    Ok(findings)
}
