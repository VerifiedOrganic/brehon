//! Git2-based implementation of the GitOperations port.
//!
//! This crate provides git operations using the `git2` crate (libgit2 bindings).
//! It handles worktree management, branching, rebasing, merging, and conflict detection.
//!
//! # Architecture
//!
//! - **ops**: Main `Git2Operations` struct implementing `GitOperations` trait
//! - **worktree**: Worktree creation, deletion, and state inspection
//! - **branch**: Branch operations (create, delete, list)
//! - **rebase**: Rebase operations with conflict detection
//! - **merge**: Merge operations with conflict reporting
//! - **integration**: Isolated worktree operations for preflight checks
//! - **diff**: Diff generation and file overlap detection
//! - **recovery**: Worktree state recovery from mid-operation states
//!
//! # Guarantees
//!
//! - Operations leave the repository in a clean state on error
//! - Conflicts are detected and reported without leaving mid-operation states
//! - All tests use temporary git repos for isolation

mod branch;
mod diff;
mod error;
mod gitignore;
mod integration;
mod merge;
mod ops;
mod rebase;
mod recovery;
mod worktree;

pub use branch::BranchOps;
pub use diff::DiffOps;
pub use error::GitError;
pub use gitignore::{
    is_legacy_brehon_dir_ignore, remove_legacy_brehon_dir_ignores, resolve_git_info_dir,
    WORKTREE_AWARE_BREHON_IGNORE_PATTERNS,
};
pub use integration::IntegrationOps;
pub use merge::MergeOps;
pub use ops::Git2Operations;
pub use rebase::RebaseOps;
pub use recovery::{RecoveryOps, RecoveryResult, StaleLockfile, WorktreeState};
pub use worktree::{ArchiveReport, CleanupReport, WorktreeOps, WorktreeStateCheck};
