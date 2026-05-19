//! Worktree state recovery from mid-operation states.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use git2::Repository;
use tracing::{debug, info, warn};

use crate::error::GitError;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LockfileKind {
    Index,
    Head,
    ModuleIndex,
    WorktreeLocked,
}

#[derive(Debug, Clone)]
struct LockfileCandidate {
    path: PathBuf,
    kind: LockfileKind,
}

#[derive(Debug, Clone)]
enum LockAgeCheckError {
    NotFound,
    Metadata(String),
    ModifiedTime(String),
    ClockSkew(String),
}

impl LockAgeCheckError {
    fn describe(&self) -> String {
        match self {
            LockAgeCheckError::NotFound => "lockfile no longer exists".to_string(),
            LockAgeCheckError::Metadata(err) => format!("metadata read failed: {err}"),
            LockAgeCheckError::ModifiedTime(err) => {
                format!("modified-time lookup failed: {err}")
            }
            LockAgeCheckError::ClockSkew(err) => {
                format!("modified time is in the future: {err}")
            }
        }
    }
}

const STALE_LOCK_MIN_AGE: Duration = Duration::from_secs(5 * 60);
const MODULE_SCAN_MAX_DEPTH: usize = 8;

/// Stale lockfile detected during recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleLockfile {
    /// Absolute path to the lockfile.
    pub path: String,
    /// Whether recovery determined this lockfile is safe to remove automatically.
    pub safe_to_remove: bool,
    /// Why the lockfile was (or was not) treated as safe.
    pub reason: String,
}

/// Worktree state enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeState {
    /// Clean state, no ongoing operations.
    Clean,
    /// Rebase in progress.
    RebaseInProgress {
        /// Branch being rebased.
        branch: Option<String>,
        /// Step in the rebase process.
        step: Option<(usize, usize)>,
    },
    /// Merge in progress.
    MergeInProgress {
        /// Branch being merged.
        branch: Option<String>,
    },
    /// Dirty working tree (uncommitted changes).
    DirtyWorkingTree {
        /// Files with changes.
        files: Vec<String>,
    },
    /// Detached HEAD state.
    DetachedHead {
        /// Current commit SHA.
        commit: Option<String>,
    },
    /// Unknown state.
    Unknown(String),
}

impl WorktreeState {
    /// Get a display string for this state.
    pub fn display(&self) -> String {
        match self {
            WorktreeState::Clean => "clean".to_string(),
            WorktreeState::RebaseInProgress { branch, step } => match (branch, step) {
                (Some(b), Some((cur, total))) => {
                    format!("rebase in progress: {} ({}/{})", b, cur, total)
                }
                (Some(b), None) => format!("rebase in progress: {}", b),
                (None, Some((cur, total))) => format!("rebase in progress ({}/{})", cur, total),
                (None, None) => "rebase in progress".to_string(),
            },
            WorktreeState::MergeInProgress { branch } => match branch {
                Some(b) => format!("merge in progress: {}", b),
                None => "merge in progress".to_string(),
            },
            WorktreeState::DirtyWorkingTree { files } => {
                format!("dirty working tree: {} file(s)", files.len())
            }
            WorktreeState::DetachedHead { commit } => match commit {
                Some(c) => format!("detached HEAD: {}", c),
                None => "detached HEAD".to_string(),
            },
            WorktreeState::Unknown(s) => format!("unknown state: {}", s),
        }
    }
}

/// Recovery operations.
pub struct RecoveryOps<'a> {
    repo: &'a Repository,
}

impl<'a> RecoveryOps<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }

    /// Detect the current worktree state.
    pub fn detect_state(&self) -> Result<Option<WorktreeState>, GitError> {
        let state = self.repo.state();

        match state {
            git2::RepositoryState::Clean => {
                if self.is_dirty()? {
                    Ok(Some(WorktreeState::DirtyWorkingTree {
                        files: self.get_dirty_files()?,
                    }))
                } else {
                    Ok(None)
                }
            }
            git2::RepositoryState::Rebase
            | git2::RepositoryState::RebaseInteractive
            | git2::RepositoryState::RebaseMerge => Ok(Some(WorktreeState::RebaseInProgress {
                branch: self.get_rebase_branch()?,
                step: self.get_rebase_step()?,
            })),
            git2::RepositoryState::Merge
            | git2::RepositoryState::Revert
            | git2::RepositoryState::RevertSequence
            | git2::RepositoryState::CherryPick
            | git2::RepositoryState::CherryPickSequence => {
                Ok(Some(WorktreeState::MergeInProgress {
                    branch: self.get_merge_branch()?,
                }))
            }
            git2::RepositoryState::ApplyMailbox | git2::RepositoryState::ApplyMailboxOrRebase => {
                Ok(Some(WorktreeState::Unknown(format!("{:?}", state))))
            }
            _ => Ok(Some(WorktreeState::Unknown(format!("{:?}", state)))),
        }
    }

    /// Check if working tree has uncommitted changes.
    pub fn is_dirty(&self) -> Result<bool, GitError> {
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true);
        options.include_ignored(false);

        let statuses = self.repo.statuses(Some(&mut options))?;

        for entry in statuses.iter() {
            match entry.status() {
                git2::Status::INDEX_NEW
                | git2::Status::INDEX_MODIFIED
                | git2::Status::INDEX_DELETED
                | git2::Status::INDEX_RENAMED
                | git2::Status::INDEX_TYPECHANGE
                | git2::Status::WT_NEW
                | git2::Status::WT_MODIFIED
                | git2::Status::WT_DELETED
                | git2::Status::WT_RENAMED
                | git2::Status::WT_TYPECHANGE
                | git2::Status::CONFLICTED => return Ok(true),
                git2::Status::CURRENT | git2::Status::IGNORED => {}
                _ => {}
            }
        }

        Ok(false)
    }

    /// Get list of dirty files.
    pub fn get_dirty_files(&self) -> Result<Vec<String>, GitError> {
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true);
        options.include_ignored(false);

        let statuses = self.repo.statuses(Some(&mut options))?;
        let mut files = Vec::new();

        for entry in statuses.iter() {
            match entry.status() {
                git2::Status::INDEX_NEW
                | git2::Status::INDEX_MODIFIED
                | git2::Status::INDEX_DELETED
                | git2::Status::INDEX_RENAMED
                | git2::Status::INDEX_TYPECHANGE
                | git2::Status::WT_NEW
                | git2::Status::WT_MODIFIED
                | git2::Status::WT_DELETED
                | git2::Status::WT_RENAMED
                | git2::Status::WT_TYPECHANGE
                | git2::Status::CONFLICTED => {
                    if let Some(path) = entry.path() {
                        files.push(path.to_string());
                    }
                }
                git2::Status::CURRENT | git2::Status::IGNORED => {}
                _ => {}
            }
        }

        Ok(files)
    }

    /// Clean up mid-rebase state.
    pub fn abort_rebase(&self) -> Result<(), GitError> {
        let state = self.repo.state();
        if matches!(
            state,
            git2::RepositoryState::Rebase
                | git2::RepositoryState::RebaseInteractive
                | git2::RepositoryState::RebaseMerge
        ) {
            info!("Aborting in-progress rebase");
            self.cleanup_state_and_verify("rebase")?;
            debug!("Rebase aborted successfully");
        }
        Ok(())
    }

    /// Clean up mid-merge state.
    pub fn abort_merge(&self) -> Result<(), GitError> {
        let state = self.repo.state();
        if matches!(
            state,
            git2::RepositoryState::Merge
                | git2::RepositoryState::Revert
                | git2::RepositoryState::RevertSequence
                | git2::RepositoryState::CherryPick
                | git2::RepositoryState::CherryPickSequence
        ) {
            info!("Aborting in-progress merge");
            self.cleanup_state_and_verify("merge")?;
            debug!("Merge aborted successfully");
        }
        Ok(())
    }

    /// Detect stale git lockfiles and classify whether they are safe to remove.
    pub fn detect_stale_lockfiles(&self) -> Result<Vec<StaleLockfile>, GitError> {
        let mut lockfiles = Vec::new();
        for candidate in self.collect_lockfile_candidates()? {
            lockfiles.push(self.classify_lockfile(&candidate)?);
        }
        Ok(lockfiles)
    }

    /// Recover to a clean state.
    ///
    /// This will abort any in-progress operations and attempt to clean up.
    /// For dirty working trees, we report but don't modify (user intervention required).
    pub fn recover(&self) -> Result<RecoveryResult, GitError> {
        debug!("Starting recovery process");

        let mut actions = Vec::new();

        if matches!(
            self.repo.state(),
            git2::RepositoryState::Rebase
                | git2::RepositoryState::RebaseInteractive
                | git2::RepositoryState::RebaseMerge
        ) {
            self.abort_rebase()?;
            actions.push("Aborted in-progress rebase".to_string());
        }

        if matches!(
            self.repo.state(),
            git2::RepositoryState::Merge
                | git2::RepositoryState::Revert
                | git2::RepositoryState::RevertSequence
                | git2::RepositoryState::CherryPick
                | git2::RepositoryState::CherryPickSequence
        ) {
            self.abort_merge()?;
            actions.push("Aborted in-progress merge".to_string());
        }

        let (removed_lock_actions, stale_lockfiles) = self.remove_safe_stale_lockfiles()?;
        actions.extend(removed_lock_actions);

        if !stale_lockfiles.is_empty() {
            warn!(
                "Detected {} stale lockfile(s) requiring manual cleanup",
                stale_lockfiles.len()
            );
        }

        if self.is_dirty()? {
            let dirty_files = self.get_dirty_files()?;
            warn!("Dirty working tree with {} files", dirty_files.len());
            return Ok(RecoveryResult {
                clean: false,
                actions,
                dirty_files: Some(dirty_files),
                stale_lockfiles,
            });
        }

        Ok(RecoveryResult {
            clean: stale_lockfiles.is_empty(),
            actions,
            dirty_files: None,
            stale_lockfiles,
        })
    }

    fn cleanup_state_and_verify(&self, operation: &str) -> Result<(), GitError> {
        self.repo.cleanup_state()?;
        Self::verify_cleanup_state_postconditions(operation, self.repo.state())
    }

    fn verify_cleanup_state_postconditions(
        operation: &str,
        state: git2::RepositoryState,
    ) -> Result<(), GitError> {
        if state != git2::RepositoryState::Clean {
            return Err(GitError::GitOperationFailed(format!(
                "cleanup_state postcondition failed after aborting {operation}: repository state is still {state:?}"
            )));
        }
        Ok(())
    }

    fn remove_safe_stale_lockfiles(&self) -> Result<(Vec<String>, Vec<StaleLockfile>), GitError> {
        let mut actions = Vec::new();
        let mut unresolved = Vec::new();

        for candidate in self.collect_lockfile_candidates()? {
            let mut lockfile = self.classify_lockfile(&candidate)?;
            if !lockfile.safe_to_remove {
                unresolved.push(lockfile);
                continue;
            }

            // Defense in depth: re-check staleness immediately before deletion.
            // This closes the TOCTOU window between classification and remove_file.
            match self.lockfile_age_for_safety(&candidate.path) {
                Ok(age) if age < STALE_LOCK_MIN_AGE => {
                    lockfile.safe_to_remove = false;
                    lockfile.reason = format!(
                        "lockfile age {} is below safety threshold {} at removal-time recheck",
                        Self::describe_duration(age),
                        Self::describe_duration(STALE_LOCK_MIN_AGE)
                    );
                    unresolved.push(lockfile);
                    continue;
                }
                Ok(_) => {}
                Err(LockAgeCheckError::NotFound) => {
                    actions.push(format!(
                        "Stale git lockfile already removed by another process: {}",
                        candidate.path.display()
                    ));
                    continue;
                }
                Err(err) => {
                    lockfile.safe_to_remove = false;
                    lockfile.reason = format!(
                        "unable to verify lockfile age at removal-time recheck: {}",
                        err.describe()
                    );
                    unresolved.push(lockfile);
                    continue;
                }
            }

            match std::fs::remove_file(&candidate.path) {
                Ok(()) => {
                    actions.push(format!(
                        "Removed stale git lockfile: {}",
                        candidate.path.display()
                    ));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    actions.push(format!(
                        "Stale git lockfile already removed by another process: {}",
                        candidate.path.display()
                    ));
                }
                Err(err) => {
                    return Err(GitError::GitOperationFailed(format!(
                        "failed to remove stale lockfile '{}': {}",
                        candidate.path.display(),
                        err
                    )));
                }
            }
        }

        Ok((actions, unresolved))
    }

    fn classify_lockfile(&self, candidate: &LockfileCandidate) -> Result<StaleLockfile, GitError> {
        let (safe_to_remove, reason) = match candidate.kind {
            LockfileKind::Index | LockfileKind::Head | LockfileKind::ModuleIndex => {
                let state = self.repo.state();
                if state != git2::RepositoryState::Clean {
                    let reason = format!(
                        "repository state is {state:?}; lockfile may belong to an active operation"
                    );
                    (false, reason)
                } else {
                    match self.lockfile_age_for_safety(&candidate.path) {
                        Ok(age) if age >= STALE_LOCK_MIN_AGE => (
                            true,
                            format!(
                                "repository state is clean and lockfile age {} meets threshold {}",
                                Self::describe_duration(age),
                                Self::describe_duration(STALE_LOCK_MIN_AGE)
                            ),
                        ),
                        Ok(age) => (
                            false,
                            format!(
                                "lockfile age {} is below safety threshold {}",
                                Self::describe_duration(age),
                                Self::describe_duration(STALE_LOCK_MIN_AGE)
                            ),
                        ),
                        Err(err) => (
                            false,
                            format!(
                                "unable to verify lockfile age; treating as unsafe: {}",
                                err.describe()
                            ),
                        ),
                    }
                }
            }
            LockfileKind::WorktreeLocked => self.classify_worktree_locked(&candidate.path)?,
        };

        Ok(StaleLockfile {
            path: candidate.path.display().to_string(),
            safe_to_remove,
            reason,
        })
    }

    fn classify_worktree_locked(&self, lock_path: &Path) -> Result<(bool, String), GitError> {
        let lock_contents = match std::fs::read_to_string(lock_path) {
            Ok(contents) => contents,
            Err(err) => {
                return Ok((
                    false,
                    format!(
                        "failed to read worktree lock contents '{}': {}; manual cleanup required",
                        lock_path.display(),
                        err
                    ),
                ))
            }
        };
        let has_reason = !lock_contents.trim().is_empty();
        let admin_dir = lock_path.parent().ok_or_else(|| {
            GitError::GitOperationFailed(format!(
                "invalid worktree lockfile path (missing parent): {}",
                lock_path.display()
            ))
        })?;
        let gitdir_ptr = admin_dir.join("gitdir");

        let worktree_gitdir = match self.read_worktree_gitdir_pointer(&gitdir_ptr) {
            Ok(pointer) => pointer,
            Err(err) => {
                return Ok((
                    false,
                    format!(
                        "failed to read worktree gitdir pointer '{}': {}; manual cleanup required",
                        gitdir_ptr.display(),
                        err
                    ),
                ))
            }
        };

        let Some(worktree_gitdir) = worktree_gitdir else {
            let reason = if has_reason {
                "worktree lock has an explicit reason and missing gitdir pointer; manual cleanup required"
                    .to_string()
            } else {
                "orphaned worktree lock without gitdir pointer and without lock reason".to_string()
            };
            return Ok((!has_reason, reason));
        };

        if worktree_gitdir.exists() {
            return Ok((
                false,
                "worktree lock points to an existing worktree gitdir; preserving lock".to_string(),
            ));
        }

        if has_reason {
            Ok((
                false,
                "worktree lock points to missing worktree but carries lock reason; manual review required"
                    .to_string(),
            ))
        } else {
            Ok((
                true,
                "worktree lock points to missing worktree and has no lock reason".to_string(),
            ))
        }
    }

    fn read_worktree_gitdir_pointer(
        &self,
        gitdir_ptr: &Path,
    ) -> Result<Option<PathBuf>, std::io::Error> {
        let contents = std::fs::read_to_string(gitdir_ptr)?;
        let raw = contents.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        let pointer = PathBuf::from(raw);
        if pointer.is_absolute() {
            Ok(Some(pointer))
        } else {
            let base = gitdir_ptr.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "gitdir pointer path has no parent directory",
                )
            })?;
            Ok(Some(base.join(pointer)))
        }
    }

    fn collect_lockfile_candidates(&self) -> Result<Vec<LockfileCandidate>, GitError> {
        let mut candidates = Vec::new();
        let local_git_dir = self.repo.path().to_path_buf();
        let common_git_dir = self.repository_common_dir();

        self.collect_primary_lockfiles(&local_git_dir, &mut candidates);
        if local_git_dir != common_git_dir {
            self.collect_primary_lockfiles(&common_git_dir, &mut candidates);
        }

        self.collect_module_lockfiles(&common_git_dir.join("modules"), &mut candidates, 0)?;
        self.collect_worktree_lockfiles(&common_git_dir.join("worktrees"), &mut candidates)?;

        // Stable ordering helps deterministic tests/logging.
        candidates.sort_by(|a, b| a.path.cmp(&b.path));
        candidates.dedup_by(|a, b| a.path == b.path);
        Ok(candidates)
    }

    fn repository_common_dir(&self) -> PathBuf {
        let git_dir = self.repo.path().to_path_buf();
        let commondir_ptr = git_dir.join("commondir");
        let Ok(contents) = std::fs::read_to_string(&commondir_ptr) else {
            return git_dir;
        };
        let raw = contents.trim();
        if raw.is_empty() {
            return git_dir;
        }
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        }
    }

    fn collect_primary_lockfiles(&self, git_dir: &Path, out: &mut Vec<LockfileCandidate>) {
        let index_lock = git_dir.join("index.lock");
        if index_lock.is_file() {
            out.push(LockfileCandidate {
                path: index_lock,
                kind: LockfileKind::Index,
            });
        }

        let head_lock = git_dir.join("HEAD.lock");
        if head_lock.is_file() {
            out.push(LockfileCandidate {
                path: head_lock,
                kind: LockfileKind::Head,
            });
        }
    }

    fn collect_module_lockfiles(
        &self,
        modules_dir: &Path,
        out: &mut Vec<LockfileCandidate>,
        depth: usize,
    ) -> Result<(), GitError> {
        if depth > MODULE_SCAN_MAX_DEPTH {
            warn!(
                "Reached max module scan depth ({}) at '{}'; skipping deeper traversal",
                MODULE_SCAN_MAX_DEPTH,
                modules_dir.display()
            );
            return Ok(());
        }

        if !modules_dir.exists() {
            return Ok(());
        }

        let entries = match std::fs::read_dir(modules_dir) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    "Failed to scan git modules directory '{}': {}",
                    modules_dir.display(),
                    err
                );
                return Ok(());
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let index_lock = path.join("index.lock");
            if index_lock.is_file() {
                out.push(LockfileCandidate {
                    path: index_lock,
                    kind: LockfileKind::ModuleIndex,
                });
            }

            // `modules/` also contains namespace directories for nested submodule paths.
            // Recurse through those namespaces, but avoid walking full git object stores.
            let looks_like_git_dir = path.join("objects").is_dir() && path.join("HEAD").exists();
            if looks_like_git_dir {
                let nested_modules = path.join("modules");
                if nested_modules.is_dir() {
                    self.collect_module_lockfiles(&nested_modules, out, depth + 1)?;
                }
            } else {
                self.collect_module_lockfiles(&path, out, depth + 1)?;
            }
        }

        Ok(())
    }

    fn lockfile_age_for_safety(&self, path: &Path) -> Result<Duration, LockAgeCheckError> {
        let metadata = std::fs::metadata(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                LockAgeCheckError::NotFound
            } else {
                LockAgeCheckError::Metadata(err.to_string())
            }
        })?;
        let modified = metadata
            .modified()
            .map_err(|err| LockAgeCheckError::ModifiedTime(err.to_string()))?;
        SystemTime::now()
            .duration_since(modified)
            .map_err(|err| LockAgeCheckError::ClockSkew(err.to_string()))
    }

    fn describe_duration(duration: Duration) -> String {
        format!("{}s", duration.as_secs())
    }

    fn collect_worktree_lockfiles(
        &self,
        worktrees_dir: &Path,
        out: &mut Vec<LockfileCandidate>,
    ) -> Result<(), GitError> {
        if !worktrees_dir.exists() {
            return Ok(());
        }

        let entries = match std::fs::read_dir(worktrees_dir) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    "Failed to scan git worktrees directory '{}': {}",
                    worktrees_dir.display(),
                    err
                );
                return Ok(());
            }
        };

        for entry in entries.flatten() {
            let admin_dir = entry.path();
            if !admin_dir.is_dir() {
                continue;
            }
            let lock_path = admin_dir.join("locked");
            if lock_path.is_file() {
                out.push(LockfileCandidate {
                    path: lock_path,
                    kind: LockfileKind::WorktreeLocked,
                });
            }
        }

        Ok(())
    }

    /// Get branch name during rebase (if available).
    fn get_rebase_branch(&self) -> Result<Option<String>, GitError> {
        let rebase_head = self.repo.path().join("rebase-merge/head-name");
        if rebase_head.exists() {
            if let Ok(contents) = std::fs::read_to_string(&rebase_head) {
                let content = contents.trim();
                if content.starts_with("refs/heads/") {
                    return Ok(Some(
                        content
                            .strip_prefix("refs/heads/")
                            .unwrap_or(content)
                            .to_string(),
                    ));
                }
                return Ok(Some(content.to_string()));
            }
        }
        Ok(None)
    }

    /// Get rebase step (current, total) if available.
    fn get_rebase_step(&self) -> Result<Option<(usize, usize)>, GitError> {
        let msg_file = self.repo.path().join("rebase-merge/msgnum");
        let total_file = self.repo.path().join("rebase-merge/end");

        if msg_file.exists() && total_file.exists() {
            let msg = std::fs::read_to_string(&msg_file).unwrap_or_default();
            let total = std::fs::read_to_string(&total_file).unwrap_or_default();

            let current = msg.trim().parse::<usize>().ok();
            let total = total.trim().parse::<usize>().ok();

            if let (Some(c), Some(t)) = (current, total) {
                return Ok(Some((c, t)));
            }
        }
        Ok(None)
    }

    /// Get branch name during merge (if available).
    fn get_merge_branch(&self) -> Result<Option<String>, GitError> {
        let merge_head = self.repo.path().join("MERGE_HEAD");
        if merge_head.exists() {
            if let Ok(contents) = std::fs::read_to_string(&merge_head) {
                let sha = contents.trim();
                if let Ok(oid) = git2::Oid::from_str(sha) {
                    if let Ok(commit) = self.repo.find_commit(oid) {
                        let refs = self.repo.references()?;
                        for reference in refs.flatten() {
                            if let Some(target) = reference.target() {
                                if target == oid {
                                    if let Some(name) = reference.shorthand() {
                                        return Ok(Some(name.to_string()));
                                    }
                                }
                            }
                        }
                        if commit.summary().is_some() {
                            return Ok(Some(commit.summary().unwrap_or("unknown").to_string()));
                        }
                    }
                }
                return Ok(Some(sha.to_string()));
            }
        }
        Ok(None)
    }
}

/// Result of a recovery operation.
#[derive(Debug)]
pub struct RecoveryResult {
    /// Whether the worktree is now clean.
    pub clean: bool,
    /// Actions taken during recovery.
    pub actions: Vec<String>,
    /// Files with dirty state (if not clean).
    pub dirty_files: Option<Vec<String>>,
    /// Stale lockfiles that were detected but not automatically removed.
    pub stale_lockfiles: Vec<StaleLockfile>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, Repository) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        (temp_dir, repo)
    }

    fn add_initial_commit(repo: &Repository) -> git2::Oid {
        let sig = Signature::now("Test", "test@example.com").expect("failed to create sig");
        let mut index = repo.index().expect("failed to get index");
        let tree_id = index.write_tree().expect("failed to write tree");
        let tree = repo.find_tree(tree_id).expect("failed to find tree");
        let commit = repo
            .commit(None, &sig, &sig, "init", &tree, &[])
            .expect("failed to commit");
        repo.reference("refs/heads/main", commit, true, "create main branch")
            .expect("failed to create ref");
        repo.set_head("refs/heads/main")
            .expect("failed to set HEAD");
        repo.checkout_head(None).expect("failed to checkout HEAD");
        commit
    }

    fn set_lockfile_age(path: &Path, age: Duration) {
        let modified = SystemTime::now()
            .checked_sub(age)
            .expect("failed to compute lockfile age");
        let mtime = filetime::FileTime::from_system_time(modified);
        filetime::set_file_mtime(path, mtime).expect("failed to set lockfile mtime");
    }

    #[test]
    fn detect_clean_state() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let ops = RecoveryOps::new(&repo);
        let state = ops.detect_state().expect("failed to detect state");

        assert!(state.is_none());
    }

    #[test]
    fn is_dirty_on_clean_worktree() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let ops = RecoveryOps::new(&repo);
        let dirty = ops.is_dirty().expect("failed to check dirty");

        assert!(!dirty);
    }

    #[test]
    fn is_dirty_on_untracked_file() {
        let (temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        std::fs::write(temp_dir.path().join("new_file.txt"), "content")
            .expect("failed to write file");

        let ops = RecoveryOps::new(&repo);
        let dirty = ops.is_dirty().expect("failed to check dirty");

        assert!(dirty);
    }

    #[test]
    fn get_dirty_files_returns_correct_files() {
        let (temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        std::fs::write(temp_dir.path().join("existing.txt"), "modified").expect("failed");
        std::fs::write(temp_dir.path().join("new.txt"), "new content").expect("failed");

        let ops = RecoveryOps::new(&repo);
        let files = ops.get_dirty_files().expect("failed to get files");

        assert!(files.contains(&"existing.txt".to_string()));
        assert!(files.contains(&"new.txt".to_string()));
    }

    #[test]
    fn recover_clean_worktree() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let ops = RecoveryOps::new(&repo);
        let result = ops.recover().expect("failed to recover");

        assert!(result.clean);
        assert!(result.dirty_files.is_none());
        assert!(result.stale_lockfiles.is_empty());
    }

    #[test]
    fn recover_dirty_worktree_reports_files() {
        let (temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        std::fs::write(temp_dir.path().join("dirty.txt"), "dirty content")
            .expect("failed to write");

        let ops = RecoveryOps::new(&repo);
        let result = ops.recover().expect("failed to recover");

        assert!(!result.clean);
        assert!(result.dirty_files.is_some());
        assert!(result
            .dirty_files
            .unwrap()
            .contains(&"dirty.txt".to_string()));
        assert!(result.stale_lockfiles.is_empty());
    }

    #[test]
    fn detect_stale_lockfiles_identifies_safe_index_and_head_locks() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let git_dir = repo.path();
        let index_lock = git_dir.join("index.lock");
        let head_lock = git_dir.join("HEAD.lock");
        std::fs::write(&index_lock, "").expect("failed to write index.lock");
        std::fs::write(&head_lock, "").expect("failed to write HEAD.lock");
        set_lockfile_age(&index_lock, STALE_LOCK_MIN_AGE + Duration::from_secs(1));
        set_lockfile_age(&head_lock, STALE_LOCK_MIN_AGE + Duration::from_secs(1));

        let ops = RecoveryOps::new(&repo);
        let lockfiles = ops
            .detect_stale_lockfiles()
            .expect("failed to detect lockfiles");

        assert_eq!(lockfiles.len(), 2);
        assert!(lockfiles.iter().all(|lock| lock.safe_to_remove));
        assert!(lockfiles
            .iter()
            .any(|lock| lock.path.ends_with("index.lock")));
        assert!(lockfiles
            .iter()
            .any(|lock| lock.path.ends_with("HEAD.lock")));
    }

    #[test]
    fn detect_stale_lockfiles_marks_recent_locks_unsafe() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let git_dir = repo.path();
        std::fs::write(git_dir.join("index.lock"), "").expect("failed to write index.lock");

        let ops = RecoveryOps::new(&repo);
        let lockfiles = ops
            .detect_stale_lockfiles()
            .expect("failed to detect lockfiles");

        assert_eq!(lockfiles.len(), 1);
        assert!(!lockfiles[0].safe_to_remove);
        assert!(lockfiles[0].reason.contains("below safety threshold"));
    }

    #[test]
    fn recover_removes_safe_stale_lockfiles() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let git_dir = repo.path();
        let index_lock = git_dir.join("index.lock");
        let head_lock = git_dir.join("HEAD.lock");
        std::fs::write(&index_lock, "").expect("failed to write index.lock");
        std::fs::write(&head_lock, "").expect("failed to write HEAD.lock");
        set_lockfile_age(&index_lock, STALE_LOCK_MIN_AGE + Duration::from_secs(1));
        set_lockfile_age(&head_lock, STALE_LOCK_MIN_AGE + Duration::from_secs(1));

        let ops = RecoveryOps::new(&repo);
        let result = ops.recover().expect("failed to recover");

        assert!(result.clean);
        assert!(!index_lock.exists());
        assert!(!head_lock.exists());
        assert!(result
            .actions
            .iter()
            .any(|action| action.contains("Removed stale git lockfile")));
        assert!(result.stale_lockfiles.is_empty());
    }

    #[test]
    fn recover_does_not_remove_recent_lockfiles() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let git_dir = repo.path();
        let index_lock = git_dir.join("index.lock");
        std::fs::write(&index_lock, "").expect("failed to write index.lock");

        let ops = RecoveryOps::new(&repo);
        let result = ops.recover().expect("failed to recover");

        assert!(!result.clean);
        assert!(index_lock.exists());
        assert_eq!(result.stale_lockfiles.len(), 1);
        assert!(!result.stale_lockfiles[0].safe_to_remove);
        assert!(result.stale_lockfiles[0]
            .reason
            .contains("below safety threshold"));
    }

    #[test]
    fn recover_preserves_reasoned_worktree_locks_for_manual_cleanup() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let worktree_lock_dir = repo.path().join("worktrees").join("demo");
        std::fs::create_dir_all(&worktree_lock_dir).expect("failed to create worktree lock dir");
        std::fs::write(worktree_lock_dir.join("locked"), "manual portable lock")
            .expect("failed to write lock");

        let ops = RecoveryOps::new(&repo);
        let result = ops.recover().expect("failed to recover");

        assert!(!result.clean);
        assert_eq!(result.stale_lockfiles.len(), 1);
        assert!(!result.stale_lockfiles[0].safe_to_remove);
        assert!(result.stale_lockfiles[0].reason.contains("manual cleanup"));
    }

    #[test]
    fn verify_cleanup_postcondition_rejects_non_clean_state() {
        let err =
            RecoveryOps::verify_cleanup_state_postconditions("merge", git2::RepositoryState::Merge)
                .expect_err("expected postcondition failure");

        assert!(err
            .to_string()
            .contains("cleanup_state postcondition failed"));
    }

    #[test]
    fn classify_lockfile_treats_metadata_failures_as_unsafe() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let candidate = LockfileCandidate {
            path: repo.path().join("index.lock"),
            kind: LockfileKind::Index,
        };

        let ops = RecoveryOps::new(&repo);
        let lockfile = ops
            .classify_lockfile(&candidate)
            .expect("classification should not fail");

        assert!(!lockfile.safe_to_remove);
        assert!(lockfile.reason.contains("unable to verify lockfile age"));
    }

    #[test]
    fn classify_worktree_lock_with_unreadable_gitdir_is_unsafe() {
        let (_temp_dir, repo) = create_test_repo();
        add_initial_commit(&repo);

        let worktree_lock_dir = repo.path().join("worktrees").join("broken");
        std::fs::create_dir_all(&worktree_lock_dir).expect("failed to create worktree lock dir");
        std::fs::write(worktree_lock_dir.join("locked"), "").expect("failed to write lock");

        let ops = RecoveryOps::new(&repo);
        let result = ops
            .classify_worktree_locked(&worktree_lock_dir.join("locked"))
            .expect("classification should succeed");

        assert!(!result.0);
        assert!(result.1.contains("manual cleanup required"));
    }

    #[test]
    fn worktree_state_display_clean() {
        let state = WorktreeState::Clean;
        assert_eq!(state.display(), "clean");
    }

    #[test]
    fn worktree_state_display_dirty() {
        let state = WorktreeState::DirtyWorkingTree {
            files: vec!["file.txt".into(), "other.rs".into()],
        };
        assert_eq!(state.display(), "dirty working tree: 2 file(s)");
    }
}
