//! Git2Operations - main implementation of GitOperations trait.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use tracing::info;

use brehon_ports::{Diff, GitOperations, MergeResult, PortError, RebaseResult};

use crate::branch::BranchOps;
use crate::diff::DiffOps;
use crate::error::GitError;
use crate::integration::IntegrationOps;
use crate::merge::MergeOps;
use crate::rebase::RebaseOps;
use crate::worktree::WorktreeOps;

/// Git operations implementation using git2 (libgit2 bindings).
pub struct Git2Operations {
    /// Shared repository handle for write operations (serialized).
    repo: Mutex<git2::Repository>,
    /// Resolved `.git` directory path used to open independent read handles,
    /// eliminating contention between read-heavy operations and long writes.
    repo_git_dir: std::path::PathBuf,
}

impl Git2Operations {
    /// Open a repository at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, GitError> {
        let path = path.as_ref().to_path_buf();
        let repo = git2::Repository::discover(&path)
            .map_err(|e| GitError::NotARepository(format!("{}: {e}", path.display())))?;
        let repo_git_dir = repo.path().to_path_buf();
        Ok(Self {
            repo: Mutex::new(repo),
            repo_git_dir,
        })
    }

    /// Create a new git repository at the given path.
    #[cfg(test)]
    pub fn init<P: AsRef<Path>>(path: P) -> Result<Self, GitError> {
        let path = path.as_ref().to_path_buf();
        let repo = git2::Repository::init(&path)?;
        let repo_git_dir = repo.path().to_path_buf();
        Ok(Self {
            repo: Mutex::new(repo),
            repo_git_dir,
        })
    }

    /// Open an independent read handle to the repository.
    ///
    /// This avoids serializing behind the write mutex, allowing concurrent
    /// read-heavy operations.
    fn open_read(&self) -> Result<git2::Repository, PortError> {
        git2::Repository::open(&self.repo_git_dir)
            .map_err(|e| PortError::Git(format!("{}: {e}", self.repo_git_dir.display())))
    }

    /// Validate that a path is a live, registered worktree distinct from the
    /// shared repository root.
    pub fn validate_worktree(&self, path: &Path) -> Result<(), GitError> {
        let repo = git2::Repository::open(&self.repo_git_dir).map_err(|e| {
            GitError::NotARepository(format!("{}: {e}", self.repo_git_dir.display()))
        })?;
        let ops = WorktreeOps::new(&repo);
        ops.validate_worktree(path)
    }
}

#[async_trait]
impl GitOperations for Git2Operations {
    async fn create_worktree(&self, branch: &str, path: &Path) -> Result<(), PortError> {
        info!(
            "Creating worktree for branch '{}' at {}",
            branch,
            path.display()
        );
        let repo = self
            .repo
            .lock()
            .map_err(|_| PortError::Git("lock error".into()))?;
        let ops = WorktreeOps::new(&repo);
        ops.create_worktree(branch, path)?;
        Ok(())
    }

    async fn create_branch(&self, name: &str, base_ref: Option<&str>) -> Result<(), PortError> {
        info!("Creating branch '{}' from {:?}", name, base_ref);
        let repo = self
            .repo
            .lock()
            .map_err(|_| PortError::Git("lock error".into()))?;
        let ops = BranchOps::new(&repo);
        ops.create_branch(name, base_ref)?;
        Ok(())
    }

    async fn delete_branch(&self, name: &str) -> Result<(), PortError> {
        info!("Deleting branch '{}'", name);
        let repo = self
            .repo
            .lock()
            .map_err(|_| PortError::Git("lock error".into()))?;
        let ops = BranchOps::new(&repo);
        ops.delete_branch(name)?;
        Ok(())
    }

    async fn remove_worktree(&self, path: &Path) -> Result<(), PortError> {
        info!("Removing worktree at {}", path.display());
        let repo = self
            .repo
            .lock()
            .map_err(|_| PortError::Git("lock error".into()))?;
        let ops = WorktreeOps::new(&repo);
        ops.remove_worktree(path)?;
        Ok(())
    }

    async fn rebase(&self, branch: &str, onto: &str) -> Result<RebaseResult, PortError> {
        info!("Rebasing '{}' onto '{}'", branch, onto);
        let repo = self
            .repo
            .lock()
            .map_err(|_| PortError::Git("lock error".into()))?;
        let ops = RebaseOps::new(&repo);
        let result = ops.rebase_branch(branch, onto)?;
        Ok(result)
    }

    async fn merge(&self, branch: &str) -> Result<MergeResult, PortError> {
        info!("Merging '{}' into current branch", branch);
        let repo = self
            .repo
            .lock()
            .map_err(|_| PortError::Git("lock error".into()))?;
        let ops = MergeOps::new(&repo);
        let result = ops.merge_branch(branch)?;
        Ok(result)
    }

    async fn diff(&self, branch: &str, base: &str) -> Result<Diff, PortError> {
        let repo = self.open_read()?;
        let ops = DiffOps::new(&repo);
        let files = ops.diff_branches(branch, base)?;
        Ok(Diff { files })
    }

    async fn has_conflicts(&self, branch: &str, base: &str) -> Result<Vec<String>, PortError> {
        let repo = self.open_read()?;
        let ops = IntegrationOps::new(&repo);
        let conflicts = ops.preview_conflicts(branch, base)?;
        Ok(conflicts)
    }

    async fn current_branch(&self) -> Result<String, PortError> {
        let repo = self.open_read()?;
        let ops = BranchOps::new(&repo);
        let name = ops.current_branch()?;
        Ok(name)
    }

    async fn checkout(&self, branch: &str) -> Result<(), PortError> {
        info!("Checking out branch '{}'", branch);
        let repo = self
            .repo
            .lock()
            .map_err(|_| PortError::Git("lock error".into()))?;
        let ops = BranchOps::new(&repo);
        ops.checkout(branch)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;

    fn setup_test_repo() -> (TempDir, Git2Operations) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Git2Operations::init(temp_dir.path()).expect("failed to init repo");

        {
            let git_repo = repo.repo.lock().unwrap_or_else(|e| e.into_inner());
            let sig =
                git2::Signature::now("Test", "test@example.com").expect("failed to create sig");
            let mut index = git_repo.index().expect("failed to get index");
            let oid = index.write_tree().expect("failed to write tree");
            let tree = git_repo.find_tree(oid).expect("failed to find tree");
            let commit = git_repo
                .commit(None, &sig, &sig, "initial commit", &tree, &[])
                .expect("failed to commit");
            git_repo
                .reference("refs/heads/main", commit, true, "create main branch")
                .expect("failed to create ref");
            git_repo
                .set_head("refs/heads/main")
                .expect("failed to set HEAD");
            git_repo
                .checkout_head(None)
                .expect("failed to checkout HEAD");
        }

        (temp_dir, repo)
    }

    #[tokio::test]
    async fn open_repository_success() {
        let (temp_dir, _) = setup_test_repo();
        let result = Git2Operations::open(temp_dir.path());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn open_nonexistent_path_fails() {
        let result = Git2Operations::open("/nonexistent/path/to/repo");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn current_branch_returns_main() {
        let (_temp_dir, repo) = setup_test_repo();
        let branch = repo
            .current_branch()
            .await
            .expect("failed to get current branch");
        assert_eq!(branch, "main");
    }

    #[tokio::test]
    async fn read_completes_while_write_lock_is_held() {
        let (_temp_dir, repo) = setup_test_repo();
        let repo = Arc::new(repo);

        let (write_lock_held_tx, write_lock_held_rx) = tokio::sync::oneshot::channel();
        let (release_write_lock_tx, release_write_lock_rx) = tokio::sync::oneshot::channel();

        let write_repo = Arc::clone(&repo);
        let writer = tokio::task::spawn_blocking(move || {
            let _write_lock = write_repo.repo.lock().unwrap_or_else(|e| e.into_inner());
            let _ = write_lock_held_tx.send(());
            let _ = release_write_lock_rx.blocking_recv();
        });

        write_lock_held_rx
            .await
            .expect("write lock holder dropped before signaling");

        let read_repo = Arc::clone(&repo);
        let read_task = tokio::spawn(async move { read_repo.current_branch().await });
        let branch = tokio::time::timeout(Duration::from_millis(250), read_task)
            .await
            .expect("read timed out while write lock was held")
            .expect("read task panicked")
            .expect("read failed while write lock was held");
        assert_eq!(branch, "main");

        let _ = release_write_lock_tx.send(());
        writer.await.expect("write lock holder panicked");
    }
}
