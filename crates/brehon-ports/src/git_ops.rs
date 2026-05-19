//! GitOperations trait for git repository operations.

use async_trait::async_trait;
use std::path::Path;

use crate::PortError;

/// Type of conflict detected in a file during rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictType {
    /// Both sides modified the same file.
    BothModified,
    /// Our side modified, their side deleted.
    ModifyDelete,
    /// Our side deleted, their side modified.
    DeleteModify,
    /// Conflict type could not be determined.
    Unknown,
}

/// Structured context for a single conflicting file during rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEntry {
    /// Path of the conflicting file.
    pub path: String,
    /// Type of conflict detected.
    pub conflict_type: ConflictType,
}

/// Fallback strategy attempted after an initial rebase conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseFallbackStrategy {
    /// No fallback was attempted; the initial rebase was aborted.
    None,
    /// Rebase retried with whitespace-level merging tolerance enabled.
    RetryWithWhitespaceTolerance,
    /// Individual cherry-pick fallback applied to non-conflicting commits.
    CherryPickRemaining,
}

/// Result of a rebase operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseResult {
    /// Rebase completed successfully.
    Success,
    /// Rebase had conflicts with structured context.
    Conflict {
        /// Per-file conflict details.
        entries: Vec<ConflictEntry>,
        /// Which fallback strategy was attempted.
        fallback_attempted: RebaseFallbackStrategy,
        /// Which fallback strategy succeeded, if any.
        fallback_succeeded: Option<RebaseFallbackStrategy>,
        /// Human-readable summary.
        summary: String,
        /// Flat list of conflicting file paths (convenience accessor).
        files: Vec<String>,
    },
}

/// Result of a merge operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// Merge completed successfully.
    Success,
    /// Merge had conflicts.
    Conflict {
        /// Files with conflicts.
        files: Vec<String>,
    },
}

/// Diff between two branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Changed files with their changes.
    pub files: Vec<FileDiff>,
}

/// Changes to a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// File path.
    pub path: String,
    /// Lines added.
    pub additions: usize,
    /// Lines removed.
    pub deletions: usize,
}

/// Trait for git repository operations.
///
/// This trait abstracts git operations needed for worktree management,
/// branching, rebasing, merging, and conflict detection.
///
/// Implementations should:
/// - Handle concurrent operations safely
/// - Provide clear error messages
/// - Leave the repository in a clean state after errors
#[async_trait]
pub trait GitOperations: Send + Sync {
    /// Create a new worktree.
    ///
    /// Creates a worktree at the specified path with the given branch
    /// checked out.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - The path already exists
    /// - The branch doesn't exist
    /// - Worktree creation fails
    async fn create_worktree(&self, branch: &str, path: &Path) -> Result<(), PortError>;

    /// Create a branch from another branch or commit.
    ///
    /// Creates a new branch starting from `base_ref` (branch name or commit hash).
    /// If `base_ref` is `None`, creates from HEAD.
    async fn create_branch(&self, name: &str, base_ref: Option<&str>) -> Result<(), PortError>;

    /// Create a new worktree with a new branch based on a base branch.
    ///
    /// Creates a worktree at the specified path, creating a new branch
    /// that starts from `base_branch` (typically the epic's integration branch),
    /// then checking out that new branch in the worktree.
    ///
    /// This is used for epic subtasks where the worker must branch from
    /// the epic's integration branch rather than from main.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - The path already exists
    /// - The base_branch doesn't exist
    /// - Branch or worktree creation fails
    ///
    /// # Cleanup
    ///
    /// If worktree creation fails after branch creation succeeds,
    /// the newly created branch is deleted to avoid leaking branches.
    async fn create_worktree_from_branch(
        &self,
        branch: &str,
        base_branch: &str,
        path: &Path,
    ) -> Result<(), PortError> {
        // Create branch from base
        self.create_branch(branch, Some(base_branch)).await?;

        // Try to create worktree; clean up branch on failure
        if let Err(e) = self.create_worktree(branch, path).await {
            // Best-effort cleanup: delete the branch we just created
            let _ = self.delete_branch(branch).await;
            return Err(e);
        }

        Ok(())
    }

    /// Delete a branch.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if the branch doesn't exist or deletion fails.
    async fn delete_branch(&self, name: &str) -> Result<(), PortError>;

    /// Remove a worktree.
    ///
    /// Removes the worktree at the specified path.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - The path doesn't exist or isn't a worktree
    /// - The worktree has uncommitted changes
    /// - Removal fails
    async fn remove_worktree(&self, path: &Path) -> Result<(), PortError>;

    /// Rebase a branch onto another branch.
    ///
    /// Performs a git rebase of `branch` onto `onto`.
    ///
    /// Returns `RebaseResult::Success` if the rebase completes cleanly,
    /// or `RebaseResult::Conflict` with structured conflict data if
    /// conflicts occur.
    ///
    /// # Fallback Strategy
    ///
    /// On initial conflict, the implementation attempts the following
    /// fallback strategies in order before returning a `Conflict` result:
    ///
    /// 1. **RetryWithWhitespaceTolerance** — Re-attempt with whitespace
    ///    tolerance enabled (renormalise + ignore-whitespace merge options).
    /// 2. **CherryPickRemaining** — Fall back to cherry-picking individual
    ///    commits; conflicting commits are skipped and reported.
    ///
    /// If a fallback succeeds, the result is `RebaseResult::Success`.
    /// If all fallbacks fail, `RebaseResult::Conflict` carries the
    /// structured conflict context including which strategies were tried.
    ///
    /// # Clean State Guarantee
    ///
    /// If a conflict occurs after all fallbacks, the worktree is left in
    /// a clean state with the rebase aborted (not in a mid-rebase state).
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - Either branch doesn't exist
    /// - The rebase fails for reasons other than conflicts
    async fn rebase(&self, branch: &str, onto: &str) -> Result<RebaseResult, PortError>;

    /// Merge a branch into the current branch.
    ///
    /// Performs a git merge of `branch` into the current HEAD.
    ///
    /// Returns `MergeResult::Success` if the merge completes cleanly,
    /// or `MergeResult::Conflict` with the conflicting files if conflicts occur.
    ///
    /// # Clean State Guarantee
    ///
    /// If a conflict occurs, the branch is left unchanged (the merge is
    /// aborted, not committed).
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - The branch doesn't exist
    /// - The merge fails for reasons other than conflicts
    async fn merge(&self, branch: &str) -> Result<MergeResult, PortError>;

    /// Get the diff between two branches.
    ///
    /// Returns the files changed and line counts for additions/deletions.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - Either branch doesn't exist
    /// - Diff generation fails
    async fn diff(&self, branch: &str, base: &str) -> Result<Diff, PortError>;

    /// Check for conflicts between two branches.
    ///
    /// Returns the files that would conflict if `branch` were merged into `base`.
    /// This is a preview - no actual merge is performed.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - Either branch doesn't exist
    /// - Conflict detection fails
    async fn has_conflicts(&self, branch: &str, base: &str) -> Result<Vec<String>, PortError>;

    /// Get the current branch name.
    ///
    /// Returns the name of the currently checked out branch.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - Not on any branch (detached HEAD)
    /// - Getting the branch name fails
    async fn current_branch(&self) -> Result<String, PortError>;

    /// Checkout a branch.
    ///
    /// Switches to the specified branch.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Git` if:
    /// - The branch doesn't exist
    /// - There are uncommitted changes
    /// - Checkout fails
    async fn checkout(&self, branch: &str) -> Result<(), PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_result_variants() {
        let success = RebaseResult::Success;
        let conflict = RebaseResult::Conflict {
            entries: vec![ConflictEntry {
                path: "src/lib.rs".into(),
                conflict_type: ConflictType::BothModified,
            }],
            fallback_attempted: RebaseFallbackStrategy::None,
            fallback_succeeded: None,
            summary: "conflict in src/lib.rs".into(),
            files: vec!["src/lib.rs".into()],
        };

        assert!(matches!(success, RebaseResult::Success));
        assert!(matches!(conflict, RebaseResult::Conflict { .. }));
    }

    #[test]
    fn merge_result_variants() {
        let success = MergeResult::Success;
        let conflict = MergeResult::Conflict {
            files: vec!["src/main.rs".into()],
        };

        assert!(matches!(success, MergeResult::Success));
        assert!(matches!(conflict, MergeResult::Conflict { .. }));
    }

    #[test]
    fn diff_structure() {
        let diff = Diff {
            files: vec![FileDiff {
                path: "src/lib.rs".into(),
                additions: 10,
                deletions: 5,
            }],
        };

        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "src/lib.rs");
        assert_eq!(diff.files[0].additions, 10);
        assert_eq!(diff.files[0].deletions, 5);
    }

    #[test]
    fn conflict_entry_classification() {
        let entry = ConflictEntry {
            path: "foo.rs".into(),
            conflict_type: ConflictType::ModifyDelete,
        };
        assert_eq!(entry.path, "foo.rs");
        assert_eq!(entry.conflict_type, ConflictType::ModifyDelete);
    }

    #[test]
    fn rebase_fallback_strategy_variants() {
        assert_eq!(RebaseFallbackStrategy::None, RebaseFallbackStrategy::None);
        assert_eq!(
            RebaseFallbackStrategy::RetryWithWhitespaceTolerance,
            RebaseFallbackStrategy::RetryWithWhitespaceTolerance
        );
        assert_eq!(
            RebaseFallbackStrategy::CherryPickRemaining,
            RebaseFallbackStrategy::CherryPickRemaining
        );
    }
}
