//! Error types for git operations.

use thiserror::Error;

/// Error type for git operations.
#[derive(Debug, Error)]
pub enum GitError {
    /// Not a git repository.
    #[error("Not a git repository: {0}")]
    NotARepository(String),

    /// Worktree creation failed.
    #[error("Worktree creation failed: {0}")]
    WorktreeCreationFailed(String),

    /// Worktree removal failed.
    #[error("Worktree removal failed: {0}")]
    WorktreeRemovalFailed(String),

    /// Branch not found.
    #[error("Branch not found: {0}")]
    BranchNotFound(String),

    /// Branch already exists.
    #[error("Branch already exists: {0}")]
    BranchAlreadyExists(String),

    /// Cannot delete current branch.
    #[error("Cannot delete current branch: {0}")]
    CannotDeleteCurrentBranch(String),

    /// Merge conflict.
    #[error("Merge conflict in files: {}", .0.join(", "))]
    MergeConflict(Vec<String>),

    /// Rebase conflict.
    #[error("Rebase conflict in files: {}", .0.join(", "))]
    RebaseConflict(Vec<String>),

    /// Dirty worktree.
    #[error("Dirty worktree: {0}")]
    DirtyWorktree(String),

    /// Mid-operation state detected.
    #[error("Repository in mid-operation state: {0}")]
    MidOperationState(String),

    /// Rebase in progress.
    #[error("Rebase in progress")]
    RebaseInProgress,

    /// Merge in progress.
    #[error("Merge in progress")]
    MergeInProgress,

    /// Git operation failed.
    #[error("Git operation failed: {0}")]
    GitOperationFailed(String),

    /// Reference not found.
    #[error("Reference not found: {0}")]
    ReferenceNotFound(String),

    /// Invalid reference name.
    #[error("Invalid reference name: {0}")]
    InvalidReferenceName(String),

    /// Detached HEAD state.
    #[error("Detached HEAD (not on any branch)")]
    DetachedHead,

    /// IO error.
    #[error("IO error: {0}")]
    IoError(String),
}

impl From<git2::Error> for GitError {
    fn from(err: git2::Error) -> Self {
        GitError::GitOperationFailed(err.message().to_string())
    }
}

impl From<std::io::Error> for GitError {
    fn from(err: std::io::Error) -> Self {
        GitError::IoError(err.to_string())
    }
}

impl From<GitError> for brehon_ports::PortError {
    fn from(err: GitError) -> Self {
        brehon_ports::PortError::Git(err.to_string())
    }
}
