//! Branch operations.

use git2::{BranchType, Repository};
use tracing::debug;

use crate::error::GitError;

/// Branch operations.
pub struct BranchOps<'a> {
    repo: &'a Repository,
}

impl<'a> BranchOps<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }

    /// Create a branch from HEAD or a commit.
    ///
    /// Creates a new branch pointing to the current HEAD (or specified commit).
    pub fn create_branch(&self, name: &str, from_commit: Option<&str>) -> Result<(), GitError> {
        debug!("Creating branch '{}' from {:?}", name, from_commit);

        let target_commit = if let Some(ref_name) = from_commit {
            let ref_path = if ref_name.starts_with("refs/") {
                ref_name.to_string()
            } else {
                format!("refs/heads/{ref_name}")
            };

            let reference = self
                .repo
                .find_reference(&ref_path)
                .map_err(|_| GitError::ReferenceNotFound(ref_name.to_string()))?;
            reference.peel_to_commit()?
        } else {
            let head = self.repo.head()?;
            head.peel_to_commit()?
        };

        self.repo.branch(name, &target_commit, false)?;
        debug!("Successfully created branch '{}'", name);
        Ok(())
    }

    /// Delete a branch.
    ///
    /// Deletes the branch. Fails if trying to delete the current branch.
    pub fn delete_branch(&self, name: &str) -> Result<(), GitError> {
        debug!("Deleting branch '{}'", name);

        let current = self.current_branch()?;
        if current == name {
            return Err(GitError::CannotDeleteCurrentBranch(name.into()));
        }

        let mut branch = self
            .repo
            .find_branch(name, BranchType::Local)
            .map_err(|_| GitError::BranchNotFound(name.into()))?;

        branch.delete()?;
        debug!("Successfully deleted branch '{}'", name);
        Ok(())
    }

    /// List all local branches.
    pub fn list_branches(&self) -> Result<Vec<String>, GitError> {
        let branches = self.repo.branches(Some(BranchType::Local))?;
        let mut result = Vec::new();

        for branch_result in branches {
            let (branch, _) = branch_result?;
            if let Some(name) = branch.name()? {
                result.push(name.to_string());
            }
        }

        Ok(result)
    }

    /// Get the current branch name.
    ///
    /// Returns the name of the currently checked out branch.
    /// Returns an error if in detached HEAD state.
    pub fn current_branch(&self) -> Result<String, GitError> {
        let head = self.repo.head()?;

        if head.is_note() || !head.is_branch() {
            return Err(GitError::DetachedHead);
        }

        let name = head
            .shorthand()
            .ok_or_else(|| GitError::GitOperationFailed("no branch name".into()))?;

        Ok(name.to_string())
    }

    /// Check if a branch exists.
    pub fn branch_exists(&self, name: &str) -> Result<bool, GitError> {
        match self.repo.find_branch(name, BranchType::Local) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Checkout a branch.
    pub fn checkout(&self, name: &str) -> Result<(), GitError> {
        debug!("Checking out branch '{}'", name);

        let branch = self
            .repo
            .find_branch(name, BranchType::Local)
            .map_err(|_| GitError::BranchNotFound(name.into()))?;

        let commit = branch.get().peel_to_commit()?;
        let tree = commit.tree()?;

        self.repo.checkout_tree(tree.as_object(), None)?;
        self.repo.set_head(&format!("refs/heads/{name}"))?;

        debug!("Successfully checked out branch '{}'", name);
        Ok(())
    }

    /// Get the commit OID for a branch.
    pub fn branch_commit_oid(&self, name: &str) -> Result<git2::Oid, GitError> {
        let branch = self
            .repo
            .find_branch(name, BranchType::Local)
            .map_err(|_| GitError::BranchNotFound(name.into()))?;

        let commit = branch.get().peel_to_commit()?;
        Ok(commit.id())
    }

    /// Rename a branch.
    pub fn rename_branch(&self, old_name: &str, new_name: &str) -> Result<(), GitError> {
        debug!("Renaming branch '{}' to '{}'", old_name, new_name);

        let mut branch = self
            .repo
            .find_branch(old_name, BranchType::Local)
            .map_err(|_| GitError::BranchNotFound(old_name.into()))?;

        branch.rename(new_name, false)?;

        debug!(
            "Successfully renamed branch '{}' to '{}'",
            old_name, new_name
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use tempfile::TempDir;

    fn setup_test_repo() -> (TempDir, Repository) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");

        let sig = Signature::now("Test", "test@example.com").expect("failed to create sig");
        let mut index = repo.index().expect("failed to get index");
        let oid = index.write_tree().expect("failed to write tree");
        let tree = repo.find_tree(oid).expect("failed to find tree");
        let commit = repo
            .commit(
                None,
                &sig,
                &sig,
                "initial commit\n\nThis is the first commit for testing branch operations.",
                &tree,
                &[],
            )
            .expect("failed to commit");
        repo.reference("refs/heads/main", commit, true, "create main branch")
            .expect("failed to create ref");
        repo.set_head("refs/heads/main")
            .expect("failed to set HEAD");
        repo.checkout_head(None).expect("failed to checkout HEAD");
        drop(tree);

        (temp_dir, repo)
    }

    #[test]
    fn current_branch_returns_main() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);
        let branch = ops.current_branch().expect("failed to get current branch");
        assert_eq!(branch, "main");
    }

    #[test]
    fn create_branch_from_head() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        ops.create_branch("feature-branch", None)
            .expect("failed to create branch");

        let branches = ops.list_branches().expect("failed to list branches");
        assert!(branches.contains(&"main".to_string()));
        assert!(branches.contains(&"feature-branch".to_string()));
    }

    #[test]
    fn create_branch_from_branch() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        ops.create_branch("feature-1", None)
            .expect("failed to create first branch");
        ops.create_branch("feature-2", Some("feature-1"))
            .expect("failed to create second branch");

        let branches = ops.list_branches().expect("failed to list branches");
        assert!(branches.contains(&"feature-1".to_string()));
        assert!(branches.contains(&"feature-2".to_string()));
    }

    #[test]
    fn branch_exists_returns_true() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        let exists = ops.branch_exists("main").expect("failed to check branch");
        assert!(exists);
    }

    #[test]
    fn branch_exists_returns_false() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        let exists = ops
            .branch_exists("nonexistent")
            .expect("failed to check branch");
        assert!(!exists);
    }

    #[test]
    fn delete_branch_removes_branch() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        ops.create_branch("to-delete", None)
            .expect("failed to create branch");
        assert!(ops.branch_exists("to-delete").expect("failed to check"));

        ops.delete_branch("to-delete")
            .expect("failed to delete branch");
        assert!(!ops.branch_exists("to-delete").expect("failed to check"));
    }

    #[test]
    fn delete_current_branch_fails() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        let result = ops.delete_branch("main");
        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(GitError::CannotDeleteCurrentBranch(_))
        ));
    }

    #[test]
    fn checkout_changes_current_branch() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        ops.create_branch("feature", None)
            .expect("failed to create branch");
        ops.checkout("feature").expect("failed to checkout branch");

        let current = ops.current_branch().expect("failed to get current branch");
        assert_eq!(current, "feature");
    }

    #[test]
    fn rename_branch_works() {
        let (_temp_dir, repo) = setup_test_repo();
        let ops = BranchOps::new(&repo);

        ops.create_branch("old-name", None)
            .expect("failed to create branch");
        ops.rename_branch("old-name", "new-name")
            .expect("failed to rename");

        assert!(!ops.branch_exists("old-name").expect("failed to check"));
        assert!(ops.branch_exists("new-name").expect("failed to check"));
    }
}
