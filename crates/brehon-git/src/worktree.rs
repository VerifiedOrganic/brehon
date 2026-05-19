//! Worktree management operations.

use std::path::Path;

use git2::{BranchType, Repository};
use tracing::{debug, warn};

use crate::error::GitError;
use crate::recovery::RecoveryOps;

pub struct CleanupReport {
    pub removed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeStateCheck {
    Clean,
    Dirty { details: String },
    MidOperation { operation: String },
    Missing,
}

impl WorktreeStateCheck {
    pub fn is_clean(&self) -> bool {
        matches!(self, WorktreeStateCheck::Clean)
    }

    pub fn display(&self) -> String {
        match self {
            WorktreeStateCheck::Clean => "clean".to_string(),
            WorktreeStateCheck::Dirty { details } => format!("dirty: {}", details),
            WorktreeStateCheck::MidOperation { operation } => {
                format!("mid-operation: {}", operation)
            }
            WorktreeStateCheck::Missing => "missing".to_string(),
        }
    }
}

pub struct ArchiveReport {
    pub archived_path: std::path::PathBuf,
    pub metadata_path: std::path::PathBuf,
}

/// Worktree operations.
pub struct WorktreeOps<'a> {
    repo: &'a Repository,
}

impl<'a> WorktreeOps<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }

    /// Create a worktree for a branch.
    ///
    /// Creates a worktree at the specified path with the given branch checked out.
    /// If the branch doesn't exist, it will be created from the current HEAD.
    pub fn create_worktree(&self, branch: &str, path: &Path) -> Result<(), GitError> {
        debug!(
            "Creating worktree for branch '{}' at '{}'",
            branch,
            path.display()
        );

        if path.exists() {
            return Err(GitError::WorktreeCreationFailed(format!(
                "path already exists: {}",
                path.display()
            )));
        }

        let recovery = RecoveryOps::new(self.repo);
        if let Some(state) = recovery.detect_state()? {
            warn!("Repository in mid-operation state: {:?}", state);
            return Err(GitError::MidOperationState(state.display()));
        }

        if recovery.is_dirty()? {
            return Err(GitError::DirtyWorktree(
                "uncommitted changes present".into(),
            ));
        }

        let branch_ref = match self.repo.find_branch(branch, BranchType::Local) {
            Ok(branch_ref) => branch_ref,
            Err(_) => {
                let head = self.repo.head()?.peel_to_commit()?;
                self.repo.branch(branch, &head, false)?
            }
        };

        let worktree_name = self.worktree_registration_name(path)?;
        if let Ok(existing) = self.repo.find_worktree(&worktree_name) {
            let existing_path = existing.path().to_path_buf();
            if existing_path == path {
                return Err(GitError::WorktreeCreationFailed(format!(
                    "worktree '{}' already exists at {}",
                    worktree_name,
                    path.display()
                )));
            }
            if !existing_path.exists() {
                let _ = existing.prune(Some(&mut git2::WorktreePruneOptions::new().valid(true)));
            } else {
                return Err(GitError::WorktreeCreationFailed(format!(
                    "worktree name '{}' is already registered at {}",
                    worktree_name,
                    existing_path.display()
                )));
            }
        }

        let mut opts = git2::WorktreeAddOptions::new();
        // Reattach to the target branch instead of coupling git's worktree
        // registration name to the branch name. This keeps metadata stable and
        // avoids nested `.git/worktrees/<branch/...>` registrations.
        opts.checkout_existing(true);
        opts.reference(Some(branch_ref.get()));
        let _worktree = self.repo.worktree(&worktree_name, path, Some(&opts))?;

        self.validate_worktree(path)?;

        debug!(
            "Successfully created worktree '{}' for branch '{}' at '{}'",
            worktree_name,
            branch,
            path.display()
        );
        Ok(())
    }

    /// Remove a worktree.
    ///
    /// Removes the worktree at the given path.
    /// Only removes clean worktrees - will fail if worktree has uncommitted changes.
    pub fn remove_worktree(&self, path: &Path) -> Result<(), GitError> {
        debug!("Removing worktree at '{}'", path.display());

        let worktree_name = self.find_worktree_name_by_path(path)?;

        let worktree = self.repo.find_worktree(&worktree_name)?;

        // Verify worktree state before attempting removal
        if let Ok(wt_path) = worktree.path().canonicalize() {
            let repo = Repository::open(&wt_path)?;
            let recovery = RecoveryOps::new(&repo);
            if recovery.is_dirty()? {
                return Err(GitError::WorktreeRemovalFailed(
                    "worktree has uncommitted changes - use archive_worktree instead".into(),
                ));
            }
            // Check for mid-operation states
            if let Some(state) = recovery.detect_state()? {
                return Err(GitError::WorktreeRemovalFailed(format!(
                    "worktree in mid-operation state: {} - abort operations first",
                    state.display()
                )));
            }
        }

        worktree.prune(Some(&mut git2::WorktreePruneOptions::new().valid(true)))?;

        // Remove directory only if prune succeeded
        if let Err(e) = std::fs::remove_dir_all(path) {
            if !path.exists() {
                debug!("Worktree directory already removed");
            } else {
                // Do not silently delete on failure - require manual intervention
                warn!(
                    "Git prune succeeded but directory removal failed: {}. Manual cleanup may be needed at {}",
                    e,
                    path.display()
                );
            }
        }

        debug!("Successfully removed worktree '{}'", worktree_name);
        Ok(())
    }

    fn find_worktree_name_by_path(&self, path: &Path) -> Result<String, GitError> {
        let canonical_path = path.canonicalize().ok();

        // First try the git2 worktrees list (non-slashed names)
        let worktrees = self.repo.worktrees()?;
        for name in worktrees.iter().flatten() {
            if let Ok(wt) = self.repo.find_worktree(name) {
                // Exact path match
                if wt.path() == path {
                    return Ok(name.to_string());
                }
                // Canonical path match (both must exist for canonicalization)
                if let (Some(canonical), Some(wt_canonical)) = (
                    canonical_path.as_ref(),
                    wt.path().canonicalize().ok().as_ref(),
                ) {
                    if canonical == wt_canonical {
                        return Ok(name.to_string());
                    }
                }
            }
        }

        // Then scan .git/worktrees for slashed branch names (nested dirs)
        // Also handles deleted worktrees by matching metadata paths
        let worktrees_dir = self.repo.path().join("worktrees");
        if worktrees_dir.exists() {
            if let Some(name) =
                self.scan_worktrees_dir_for_path(&worktrees_dir, &canonical_path, path)?
            {
                return Ok(name);
            }
        }

        Err(GitError::WorktreeRemovalFailed(format!(
            "no worktree found at path: {}",
            path.display()
        )))
    }

    fn worktree_registration_name(&self, path: &Path) -> Result<String, GitError> {
        let repo_root = self
            .repo
            .workdir()
            .or_else(|| self.repo.path().parent())
            .ok_or_else(|| GitError::WorktreeCreationFailed("repository has no workdir".into()))?;
        let relative = path.strip_prefix(repo_root).unwrap_or(path);
        let mut parts = Vec::new();
        for component in relative.components() {
            let raw = component.as_os_str().to_string_lossy();
            let sanitized: String = raw
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                        ch
                    } else {
                        '-'
                    }
                })
                .collect();
            let trimmed = sanitized.trim_matches('-');
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }

        if parts.is_empty() {
            return Err(GitError::WorktreeCreationFailed(format!(
                "cannot derive a stable worktree name from path '{}'",
                path.display()
            )));
        }

        Ok(parts.join("__"))
    }

    fn scan_worktrees_dir_for_path(
        &self,
        dir: &Path,
        canonical_path: &Option<std::path::PathBuf>,
        original_path: &Path,
    ) -> Result<Option<String>, GitError> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            GitError::WorktreeRemovalFailed(format!("failed to read worktrees dir: {}", e))
        })?;

        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                // Check if this is a worktree metadata dir (has gitdir file)
                let gitdir_path = entry_path.join("gitdir");
                if gitdir_path.exists() {
                    // For slashed branches, the name is the parent dir + "/" + this dir
                    let worktree_name = self.reconstruct_worktree_name(&entry_path);

                    // Try to match via find_worktree (for existing worktrees) or via metadata (for deleted)
                    let matched = self.match_worktree_by_path(
                        &worktree_name,
                        original_path,
                        canonical_path,
                        &entry_path,
                    )?;
                    if matched {
                        return Ok(Some(worktree_name));
                    }
                } else {
                    // This might be a parent dir (for slashed branches), recurse
                    if let Some(name) = self.scan_worktrees_dir_for_path(
                        &entry_path,
                        canonical_path,
                        original_path,
                    )? {
                        return Ok(Some(name));
                    }
                }
            }
        }

        Ok(None)
    }

    fn match_worktree_by_path(
        &self,
        worktree_name: &str,
        original_path: &Path,
        canonical_path: &Option<std::path::PathBuf>,
        metadata_path: &Path,
    ) -> Result<bool, GitError> {
        // First try to match via find_worktree (for existing worktrees)
        if let Ok(wt) = self.repo.find_worktree(worktree_name) {
            // Exact path match
            if wt.path() == original_path {
                return Ok(true);
            }
            // Canonical path match (both must exist for canonicalization)
            if let (Some(canonical), Some(wt_canonical)) = (
                canonical_path.as_ref(),
                wt.path().canonicalize().ok().as_ref(),
            ) {
                if canonical == wt_canonical {
                    return Ok(true);
                }
            }
        }

        // Fallback for deleted directories: read from metadata
        // The metadata stores the canonical path (e.g., /private/var/... on macOS)
        if let Ok(WorktreeMetadata {
            worktree_path: stored_path,
        }) = self.read_worktree_metadata(metadata_path)
        {
            // Direct match with original path
            if stored_path == original_path {
                return Ok(true);
            }
            // Canonical match if both can be canonicalized
            if let (Some(canonical), Some(stored_canonical)) = (
                canonical_path.as_ref(),
                stored_path.canonicalize().ok().as_ref(),
            ) {
                if canonical == stored_canonical {
                    return Ok(true);
                }
            }
            // Fallback: when input path doesn't exist (canonical_path is None),
            // try to canonicalize parent directory and append the final component
            if canonical_path.is_none() {
                // stored_path is already canonical, original_path is not
                // Try to resolve original_path by canonicalizing its parent
                if let Some(parent) = original_path.parent() {
                    if let Some(file_name) = original_path.file_name() {
                        if let Ok(canonical_parent) = parent.canonicalize() {
                            let resolved_path = canonical_parent.join(file_name);
                            if stored_path == resolved_path {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }

        Ok(false)
    }

    fn reconstruct_worktree_name(&self, metadata_dir: &Path) -> String {
        let worktrees_dir = self.repo.path().join("worktrees");
        let relative = metadata_dir
            .strip_prefix(&worktrees_dir)
            .unwrap_or(metadata_dir);
        relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn list_worktrees(&self) -> Result<Vec<(String, std::path::PathBuf)>, GitError> {
        let mut result = Vec::new();

        // First, get worktrees from git2's list (non-slashed names)
        let worktrees = self.repo.worktrees()?;
        for name in worktrees.iter().flatten() {
            if let Ok(worktree) = self.repo.find_worktree(name) {
                result.push((name.to_string(), worktree.path().to_path_buf()));
            }
        }

        // Then, scan .git/worktrees for slashed branch names (nested dirs)
        let worktrees_dir = self.repo.path().join("worktrees");
        if worktrees_dir.exists() {
            self.scan_and_list_worktrees(&worktrees_dir, &mut result)?;
        }

        Ok(result)
    }

    fn scan_and_list_worktrees(
        &self,
        dir: &Path,
        result: &mut Vec<(String, std::path::PathBuf)>,
    ) -> Result<(), GitError> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            GitError::WorktreeRemovalFailed(format!("failed to read worktrees dir: {}", e))
        })?;

        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                let gitdir_path = entry_path.join("gitdir");
                if gitdir_path.exists() {
                    // This is a worktree - try to find it
                    let worktree_name = self.reconstruct_worktree_name(&entry_path);
                    if result.iter().all(|(name, _)| name != &worktree_name) {
                        if let Ok(wt) = self.repo.find_worktree(&worktree_name) {
                            result.push((worktree_name, wt.path().to_path_buf()));
                        }
                    }
                } else {
                    // Parent dir for slashed branches, recurse
                    self.scan_and_list_worktrees(&entry_path, result)?;
                }
            }
        }

        Ok(())
    }

    pub fn is_worktree_clean(&self, path: &Path) -> Result<bool, GitError> {
        let worktree_name = self.find_worktree_name_by_path(path)?;
        let worktree = self.repo.find_worktree(&worktree_name)?;

        if let Ok(wt_path) = worktree.path().canonicalize() {
            let repo = Repository::open(&wt_path)?;
            let recovery = RecoveryOps::new(&repo);
            return Ok(!recovery.is_dirty()?);
        }

        Err(GitError::WorktreeRemovalFailed(format!(
            "no worktree at path: {}",
            path.display()
        )))
    }

    pub fn cleanup_all_worktrees(&self) -> Result<CleanupReport, GitError> {
        let worktrees = self.list_worktrees()?;
        let mut report = CleanupReport {
            removed: Vec::new(),
            failed: Vec::new(),
        };

        for (name, path) in worktrees {
            match self.remove_worktree(&path) {
                Ok(()) => report.removed.push(name),
                Err(e) => report.failed.push((name, e.to_string())),
            }
        }

        Ok(report)
    }

    pub fn cleanup_stale_metadata(&self) -> Result<Vec<String>, GitError> {
        let worktrees_dir = self.repo.path().join("worktrees");
        if !worktrees_dir.exists() {
            return Ok(Vec::new());
        }

        let mut cleaned = Vec::new();
        self.cleanup_stale_metadata_recursive(&worktrees_dir, &mut cleaned)?;
        Ok(cleaned)
    }

    fn cleanup_stale_metadata_recursive(
        &self,
        dir: &Path,
        cleaned: &mut Vec<String>,
    ) -> Result<(), GitError> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            GitError::WorktreeRemovalFailed(format!("failed to read worktrees dir: {}", e))
        })?;

        for entry in entries.flatten() {
            let metadata_path = entry.path();
            if !metadata_path.is_dir() {
                continue;
            }

            // Check if this is a worktree metadata dir (has gitdir file)
            let gitdir_path = metadata_path.join("gitdir");
            if gitdir_path.exists() {
                if let Ok(WorktreeMetadata { worktree_path }) =
                    self.read_worktree_metadata(&metadata_path)
                {
                    if !worktree_path.exists() {
                        let worktree_name = self.reconstruct_worktree_name(&metadata_path);

                        // Try to prune via git2 first (cleans up branch refs)
                        let pruned = if let Ok(wt) = self.repo.find_worktree(&worktree_name) {
                            wt.prune(Some(&mut git2::WorktreePruneOptions::new().valid(true)))
                                .is_ok()
                        } else {
                            false
                        };

                        // If prune failed or metadata too corrupt, fall back to remove_dir_all
                        if !pruned {
                            if let Err(e) = std::fs::remove_dir_all(&metadata_path) {
                                warn!(
                                    "Failed to remove stale worktree metadata at {}: {}",
                                    metadata_path.display(),
                                    e
                                );
                                continue;
                            }
                        }
                        cleaned.push(worktree_name);
                    }
                }
            } else {
                // This might be a parent dir (for slashed branches), recurse
                self.cleanup_stale_metadata_recursive(&metadata_path, cleaned)?;
            }
        }

        Ok(())
    }

    fn read_worktree_metadata(&self, metadata_path: &Path) -> Result<WorktreeMetadata, GitError> {
        let git_dir_file = metadata_path.join("gitdir");
        let git_dir = std::fs::read_to_string(&git_dir_file).map_err(|e| {
            GitError::WorktreeRemovalFailed(format!("failed to read gitdir: {}", e))
        })?;

        // gitdir contains path like /path/to/worktree/.git
        // The worktree directory is the parent of .git
        let worktree_git_dir = Path::new(git_dir.trim());
        let worktree_path = worktree_git_dir
            .parent()
            .ok_or_else(|| GitError::WorktreeRemovalFailed("invalid worktree path".into()))?
            .to_path_buf();

        Ok(WorktreeMetadata { worktree_path })
    }

    /// Check the state of a worktree for safety before operations like reassignment.
    pub fn worktree_state_check(&self, path: &Path) -> Result<WorktreeStateCheck, GitError> {
        debug!("Checking worktree state at '{}'", path.display());

        if !path.exists() {
            return Ok(WorktreeStateCheck::Missing);
        }

        let worktree_name = match self.find_worktree_name_by_path(path) {
            Ok(name) => name,
            Err(_) => return Ok(WorktreeStateCheck::Missing),
        };

        let worktree = self.repo.find_worktree(&worktree_name)?;
        let wt_path = worktree.path().canonicalize().map_err(|e| {
            GitError::WorktreeRemovalFailed(format!("failed to canonicalize: {}", e))
        })?;

        let wt_repo = Repository::open(&wt_path)?;
        let recovery = RecoveryOps::new(&wt_repo);

        // Check for dirty working tree first (uncommitted changes)
        if recovery.is_dirty()? {
            let dirty_files = recovery.get_dirty_files()?;
            let details = if dirty_files.len() <= 5 {
                dirty_files.join(", ")
            } else {
                format!("{} files", dirty_files.len())
            };
            return Ok(WorktreeStateCheck::Dirty { details });
        }

        // Check for mid-operation state (rebase, merge, etc.)
        if let Some(state) = recovery.detect_state()? {
            return Ok(WorktreeStateCheck::MidOperation {
                operation: state.display(),
            });
        }

        Ok(WorktreeStateCheck::Clean)
    }

    pub fn validate_worktree(&self, path: &Path) -> Result<(), GitError> {
        let state = self.worktree_state_check(path)?;
        if state != WorktreeStateCheck::Clean {
            return Err(GitError::WorktreeCreationFailed(format!(
                "worktree '{}' failed validation: {}",
                path.display(),
                state.display()
            )));
        }

        let repo_root = self
            .repo
            .workdir()
            .or_else(|| self.repo.path().parent())
            .ok_or_else(|| GitError::WorktreeCreationFailed("repository has no workdir".into()))?
            .canonicalize()
            .map_err(|e| {
                GitError::WorktreeCreationFailed(format!(
                    "failed to canonicalize repo root '{}': {e}",
                    path.display()
                ))
            })?;
        let worktree_root = path.canonicalize().map_err(|e| {
            GitError::WorktreeCreationFailed(format!(
                "failed to canonicalize worktree '{}': {e}",
                path.display()
            ))
        })?;
        if worktree_root == repo_root {
            return Err(GitError::WorktreeCreationFailed(format!(
                "worktree '{}' resolves to the shared repository root",
                path.display()
            )));
        }

        let worktree_name = self.find_worktree_name_by_path(path)?;
        let worktree = self.repo.find_worktree(&worktree_name)?;
        let registered_root = worktree.path().canonicalize().map_err(|e| {
            GitError::WorktreeCreationFailed(format!(
                "failed to canonicalize registered worktree '{}': {e}",
                path.display()
            ))
        })?;
        if registered_root != worktree_root {
            return Err(GitError::WorktreeCreationFailed(format!(
                "worktree '{}' is registered at '{}' instead",
                path.display(),
                registered_root.display()
            )));
        }

        Ok(())
    }

    /// Archive a worktree to the _archived directory instead of deleting it.
    pub fn archive_worktree(
        &self,
        path: &Path,
        archive_base: &Path,
        task_id: &str,
    ) -> Result<ArchiveReport, GitError> {
        // Find worktree name BEFORE moving the directory
        let worktree_name = self.find_worktree_name_by_path(path)?;

        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let archive_name = format!("{}_{}", task_id, timestamp);
        let archive_path = archive_base.join(&archive_name);

        debug!(
            "Archiving worktree '{}' from '{}' to '{}'",
            worktree_name,
            path.display(),
            archive_path.display()
        );

        std::fs::create_dir_all(archive_base).map_err(|e| {
            GitError::WorktreeRemovalFailed(format!("failed to create archive dir: {}", e))
        })?;

        // Write metadata about the archive BEFORE moving
        let metadata_path = archive_path.join(".brehon_archive_metadata.json");
        let metadata = serde_json::json!({
            "original_path": path.to_string_lossy(),
            "worktree_name": worktree_name,
            "task_id": task_id,
            "archived_at": chrono::Utc::now().to_rfc3339(),
            "archive_reason": "reassignment",
        });

        // Move the worktree directory
        std::fs::rename(path, &archive_path).map_err(|e| {
            GitError::WorktreeRemovalFailed(format!("failed to move worktree: {}", e))
        })?;

        std::fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        )
        .map_err(|e| GitError::WorktreeRemovalFailed(format!("failed to write metadata: {}", e)))?;

        // Clean up the git worktree reference AFTER moving
        if let Ok(wt) = self.repo.find_worktree(&worktree_name) {
            let _ = wt.prune(Some(&mut git2::WorktreePruneOptions::new().valid(true)));
        }

        debug!(
            "Successfully archived worktree to '{}'",
            archive_path.display()
        );

        Ok(ArchiveReport {
            archived_path: archive_path,
            metadata_path,
        })
    }
}

struct WorktreeMetadata {
    worktree_path: std::path::PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use tempfile::TempDir;

    fn setup_test_repo() -> (TempDir, Repository) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");

        let sig = git2::Signature::now("Test", "test@example.com").expect("failed to create sig");
        let mut index = repo.index().expect("failed to get index");
        let oid = index.write_tree().expect("failed to write tree");
        let tree = repo.find_tree(oid).expect("failed to find tree");
        let commit = repo.commit(None, &sig, &sig, "initial commit\n\nThis is the first commit in the test repository,\ncreated to set up a known state for worktree tests.", &tree, &[]).expect("failed to commit");
        repo.reference("refs/heads/main", commit, true, "create main branch")
            .expect("failed to create ref");
        repo.set_head("refs/heads/main")
            .expect("failed to set HEAD");
        repo.checkout_head(None).expect("failed to checkout HEAD");
        drop(tree);

        (temp_dir, repo)
    }

    #[test]
    fn list_empty_worktrees() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let worktrees = ops.list_worktrees().expect("failed to list worktrees");
        assert!(worktrees.is_empty());
    }

    #[test]
    fn create_worktree_creates_directory() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path = temp_dir.path().join("test-worktree");

        let result = ops.create_worktree(&branch_name, &worktree_path);

        assert!(result.is_ok(), "result was: {:?}", result);
        assert!(worktree_path.exists());
    }

    #[test]
    fn create_worktree_same_branch_twice_fails() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let path1 = temp_dir.path().join("worktree1");
        let path2 = temp_dir.path().join("worktree2");

        ops.create_worktree(&branch_name, &path1)
            .expect("first create should succeed");
        let result = ops.create_worktree(&branch_name, &path2);

        assert!(result.is_err());
    }

    #[test]
    fn create_worktree_existing_path_fails() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);

        let worktree_path = temp_dir.path().join("existing-dir");
        std::fs::create_dir_all(&worktree_path).expect("failed to create dir");

        let result = ops.create_worktree("test-branch-existing", &worktree_path);
        assert!(result.is_err());
    }

    #[test]
    fn create_worktree_allows_branch_names_with_slashes() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = "brehon/worker-1";
        let worktree_path = temp_dir.path().join("nested-worktree");

        let result = ops.create_worktree(branch_name, &worktree_path);

        assert!(result.is_ok(), "result was: {:?}", result);
        assert!(worktree_path.exists());
    }

    #[test]
    fn create_worktree_reuses_existing_branch_without_recreating_ref() {
        let (temp_dir, repo) = setup_test_repo();
        let branch_name = "brehon/supervisor/claude-code";
        let branch_ops = crate::branch::BranchOps::new(&repo);
        branch_ops
            .create_branch(branch_name, None)
            .expect("failed to create branch");

        let ops = WorktreeOps::new(&repo);
        let worktree_path = temp_dir.path().join("existing-branch-worktree");
        let result = ops.create_worktree(branch_name, &worktree_path);

        assert!(result.is_ok(), "should reuse existing branch: {result:?}");
        assert!(worktree_path.exists());
    }

    #[test]
    fn remove_worktree_deletes_directory() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path = temp_dir.path().join("deleteme-worktree");

        ops.create_worktree(&branch_name, &worktree_path)
            .expect("create should succeed");
        assert!(worktree_path.exists());

        ops.remove_worktree(&worktree_path)
            .expect("remove should succeed");
        assert!(!worktree_path.exists());
    }

    #[test]
    fn create_and_remove_slashed_branch_worktree() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = "brehon/worker-1";
        let worktree_path = temp_dir.path().join("slashed-worktree");

        ops.create_worktree(branch_name, &worktree_path)
            .expect("create should succeed");
        assert!(worktree_path.exists());

        ops.remove_worktree(&worktree_path)
            .expect("remove should succeed");
        assert!(!worktree_path.exists());

        let worktrees_dir = repo.path().join("worktrees");
        assert!(!worktrees_dir.join("brehon").join("worker-1").exists());
    }

    #[test]
    fn remove_worktree_already_deleted_dir() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path = temp_dir.path().join("deleted-worktree");

        ops.create_worktree(&branch_name, &worktree_path)
            .expect("create should succeed");

        std::fs::remove_dir_all(&worktree_path).expect("manual delete should work");

        ops.remove_worktree(&worktree_path)
            .expect("remove should still succeed even with deleted dir");

        let worktrees = repo.worktrees().expect("failed to list worktrees");
        assert!(!worktrees.iter().flatten().any(|n| n == branch_name));
    }

    #[test]
    fn remove_worktree_does_not_match_wrong_deleted_worktree() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name1 = format!("test-{}", uuid::Uuid::new_v4());
        let branch_name2 = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path1 = temp_dir.path().join("worktree1");
        let worktree_path2 = temp_dir.path().join("worktree2");

        ops.create_worktree(&branch_name1, &worktree_path1)
            .expect("create should succeed");
        ops.create_worktree(&branch_name2, &worktree_path2)
            .expect("create should succeed");

        // Delete both directories manually
        std::fs::remove_dir_all(&worktree_path1).expect("manual delete should work");
        std::fs::remove_dir_all(&worktree_path2).expect("manual delete should work");

        // Removing the first should NOT accidentally match the second
        ops.remove_worktree(&worktree_path1)
            .expect("remove should succeed");

        // Verify only the first registration was removed. The second worktree
        // directory is also deleted, so list_worktrees() will not surface it;
        // check the raw registered worktree count instead.
        let worktrees = repo.worktrees().expect("failed to list worktrees");
        let names: Vec<_> = worktrees.iter().flatten().collect();
        assert_eq!(
            names.len(),
            1,
            "only one worktree registration should remain"
        );

        // Clean up second worktree
        ops.remove_worktree(&worktree_path2)
            .expect("remove second should succeed");
    }

    #[test]
    fn cleanup_stale_metadata_removes_orphans() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path = temp_dir.path().join("orphan-worktree");

        ops.create_worktree(&branch_name, &worktree_path)
            .expect("create should succeed");

        std::fs::remove_dir_all(&worktree_path).expect("manual delete should work");

        let cleaned = ops
            .cleanup_stale_metadata()
            .expect("cleanup should succeed");
        assert!(
            !cleaned.is_empty(),
            "should have cleaned at least one worktree"
        );
    }

    #[test]
    fn cleanup_all_worktrees_removes_everything() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);

        let paths: Vec<_> = (0..3)
            .map(|i| {
                (
                    format!("test-{}", uuid::Uuid::new_v4()),
                    temp_dir.path().join(format!("worktree-{}", i)),
                )
            })
            .collect();

        for (branch, path) in &paths {
            ops.create_worktree(branch, path)
                .expect("create should succeed");
        }

        let report = ops.cleanup_all_worktrees().expect("cleanup should succeed");
        assert_eq!(report.removed.len(), 3);
        assert!(report.failed.is_empty());

        for (_, path) in &paths {
            assert!(!path.exists());
        }

        let remaining = ops.list_worktrees().expect("list should succeed");
        assert!(remaining.is_empty());
    }

    #[test]
    fn worktree_state_check_clean() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path = temp_dir.path().join("clean-worktree");

        ops.create_worktree(&branch_name, &worktree_path)
            .expect("create should succeed");

        let state = ops
            .worktree_state_check(&worktree_path)
            .expect("state check should succeed");
        assert!(
            state.is_clean(),
            "Clean worktree should be reported as clean, got: {:?}",
            state
        );
    }

    #[test]
    fn worktree_state_check_dirty() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path = temp_dir.path().join("dirty-worktree");

        ops.create_worktree(&branch_name, &worktree_path)
            .expect("create should succeed");

        std::fs::write(worktree_path.join("new_file.txt"), "content").expect("failed to write");

        let state = ops
            .worktree_state_check(&worktree_path)
            .expect("state check should succeed");
        assert!(
            matches!(state, WorktreeStateCheck::Dirty { .. }),
            "Dirty worktree should be reported as dirty, got: {:?}",
            state
        );
    }

    #[test]
    fn worktree_state_check_missing() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let nonexistent = temp_dir.path().join("does-not-exist");

        let state = ops
            .worktree_state_check(&nonexistent)
            .expect("state check should succeed");
        assert_eq!(state, WorktreeStateCheck::Missing);
    }

    #[test]
    fn archive_worktree_moves_directory() {
        let (temp_dir, repo) = setup_test_repo();
        let ops = WorktreeOps::new(&repo);
        let branch_name = format!("test-{}", uuid::Uuid::new_v4());
        let worktree_path = temp_dir.path().join("to-archive");
        let archive_base = temp_dir.path().join("archived");

        ops.create_worktree(&branch_name, &worktree_path)
            .expect("create should succeed");

        std::fs::write(worktree_path.join("test.txt"), "content").expect("failed to write");

        let result = ops
            .archive_worktree(&worktree_path, &archive_base, "T-test")
            .expect("archive should succeed");

        assert!(!worktree_path.exists(), "Original path should be removed");
        assert!(result.archived_path.exists(), "Archive path should exist");
        assert!(result.metadata_path.exists(), "Metadata file should exist");
    }
}
