//! Integration worktree operations for preflight checks.

use git2::Repository;
use tracing::debug;

use crate::error::GitError;
use crate::rebase::RebaseOps;

/// Integration operations.
pub struct IntegrationOps<'a> {
    repo: &'a Repository,
}

impl<'a> IntegrationOps<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }

    fn path_to_string(path: &std::path::Path) -> String {
        path.to_string_lossy().to_string()
    }

    /// Preview merge conflicts without modifying anything.
    ///
    /// Returns files that WOULD conflict if `branch` were merged into `base`.
    /// This is computed by analyzing diffs from merge base.
    pub fn preview_conflicts(&self, branch: &str, base: &str) -> Result<Vec<String>, GitError> {
        debug!("Previewing conflicts between '{}' and '{}'", branch, base);

        let branch_ref = format!("refs/heads/{branch}");
        let base_ref = format!("refs/heads/{base}");

        let branch_commit = self.repo.find_reference(&branch_ref)?.peel_to_commit()?;
        let base_commit = self.repo.find_reference(&base_ref)?.peel_to_commit()?;

        let merge_base = self.repo.merge_base(branch_commit.id(), base_commit.id())?;

        let base_commit_obj = self.repo.find_commit(base_commit.id())?;
        let branch_commit_obj = self.repo.find_commit(branch_commit.id())?;

        let base_tree = base_commit_obj.tree()?;
        let branch_tree = branch_commit_obj.tree()?;
        let merge_base_tree = self.repo.find_commit(merge_base)?.tree()?;

        let base_diff =
            self.repo
                .diff_tree_to_tree(Some(&merge_base_tree), Some(&base_tree), None)?;
        let branch_diff =
            self.repo
                .diff_tree_to_tree(Some(&merge_base_tree), Some(&branch_tree), None)?;

        let mut base_changed_files = std::collections::HashSet::new();
        let mut branch_changed_files = std::collections::HashSet::new();

        base_diff.foreach(
            &mut |delta, _| {
                if let Some(path) = delta.new_file().path() {
                    base_changed_files.insert(Self::path_to_string(path));
                } else if let Some(path) = delta.old_file().path() {
                    base_changed_files.insert(Self::path_to_string(path));
                }
                true
            },
            None,
            None,
            None,
        )?;

        branch_diff.foreach(
            &mut |delta, _| {
                if let Some(path) = delta.new_file().path() {
                    branch_changed_files.insert(Self::path_to_string(path));
                } else if let Some(path) = delta.old_file().path() {
                    branch_changed_files.insert(Self::path_to_string(path));
                }
                true
            },
            None,
            None,
            None,
        )?;

        let conflicts: Vec<String> = base_changed_files
            .intersection(&branch_changed_files)
            .cloned()
            .collect();

        debug!("Found {} potential conflicts", conflicts.len());
        Ok(conflicts)
    }

    /// Test if a merge can be performed cleanly in isolation.
    ///
    /// Creates a temporary worktree, attempts the merge there,
    /// and returns whether conflicts were found.
    pub fn test_merge_clean(&self, branch: &str, base: &str) -> Result<bool, GitError> {
        debug!("Testing merge of '{}' into '{}' in isolation", branch, base);

        let conflicts = self.preview_conflicts(branch, base)?;
        Ok(conflicts.is_empty())
    }

    /// Test if a rebase can be performed cleanly in isolation.
    ///
    /// Creates a temporary worktree, attempts the rebase there,
    /// and returns whether conflicts were found.
    pub fn test_rebase_clean(&self, branch: &str, onto: &str) -> Result<bool, GitError> {
        debug!(
            "Testing rebase of '{}' onto '{}' in isolation",
            branch, onto
        );

        let rebase_ops = RebaseOps::new(self.repo);
        let conflicts = rebase_ops.preview_rebase_conflicts(branch, onto)?;
        Ok(conflicts.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, Repository) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        (temp_dir, repo)
    }

    #[test]
    fn preview_conflicts_no_conflicts() {
        let (_temp_dir, repo) = create_test_repo();

        let base_tree = repo
            .treebuilder(None)
            .expect("failed to create builder")
            .write()
            .expect("failed");
        let base = repo.find_tree(base_tree).expect("failed");
        let sig = Signature::now("Test", "test@example.com").expect("failed");
        let init = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "init", &base, &[])
            .expect("failed");

        repo.branch("feature", &repo.find_commit(init).expect("failed"), false)
            .expect("failed to create branch");

        let ops = IntegrationOps::new(&repo);
        let conflicts = ops
            .preview_conflicts("feature", "main")
            .expect("preview should work");

        assert!(conflicts.is_empty());
    }

    #[test]
    fn preview_conflicts_detects_overlapping_changes() {
        let (_temp_dir, repo) = create_test_repo();
        let sig = Signature::now("Test", "test@example.com").expect("failed");

        let base_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob = repo.blob(b"base content").expect("failed");
            builder.insert("file.txt", blob, 0o100644).expect("failed");
            builder.write().expect("failed")
        };
        let tree = repo.find_tree(base_tree).expect("failed");
        let base_commit = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "base", &tree, &[])
            .expect("failed");

        let main_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob = repo.blob(b"main modified").expect("failed");
            builder.insert("file.txt", blob, 0o100644).expect("failed");
            builder.write().expect("failed")
        };
        let main_tree = repo.find_tree(main_tree).expect("failed");
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "main change",
            &main_tree,
            &[&repo.find_commit(base_commit).expect("failed")],
        )
        .expect("failed");

        repo.branch(
            "feature",
            &repo.find_commit(base_commit).expect("failed"),
            false,
        )
        .expect("failed");

        let feature_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob = repo.blob(b"feature modified").expect("failed");
            builder.insert("file.txt", blob, 0o100644).expect("failed");
            builder.write().expect("failed")
        };
        let feature_tree = repo.find_tree(feature_tree).expect("failed");
        repo.commit(
            Some("refs/heads/feature"),
            &sig,
            &sig,
            "feature change",
            &feature_tree,
            &[&repo.find_commit(base_commit).expect("failed")],
        )
        .expect("failed");

        let ops = IntegrationOps::new(&repo);
        let conflicts = ops
            .preview_conflicts("feature", "main")
            .expect("preview should work");

        assert!(conflicts.contains(&"file.txt".to_string()));
    }

    #[test]
    fn test_merge_clean_detects_no_conflicts() {
        let (_temp_dir, repo) = create_test_repo();
        let sig = Signature::now("Test", "test@example.com").expect("failed");

        let base_tree = repo
            .treebuilder(None)
            .expect("failed")
            .write()
            .expect("failed");
        let tree = repo.find_tree(base_tree).expect("failed");
        let init = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .expect("failed");

        repo.branch("feature", &repo.find_commit(init).expect("failed"), false)
            .expect("failed");

        let ops = IntegrationOps::new(&repo);
        let is_clean = ops
            .test_merge_clean("feature", "main")
            .expect("test should work");

        assert!(is_clean);
    }

    #[test]
    fn test_merge_clean_detects_conflicts() {
        let (_temp_dir, repo) = create_test_repo();
        let sig = Signature::now("Test", "test@example.com").expect("failed");

        let base_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob = repo.blob(b"base").expect("failed");
            builder.insert("file.txt", blob, 0o100644).expect("failed");
            builder.write().expect("failed")
        };
        let tree = repo.find_tree(base_tree).expect("failed");
        let base = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "base", &tree, &[])
            .expect("failed");
        let base_commit = repo.find_commit(base).expect("failed");

        let main_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob = repo.blob(b"main").expect("failed");
            builder.insert("file.txt", blob, 0o100644).expect("failed");
            builder.write().expect("failed")
        };
        let main_tree = repo.find_tree(main_tree).expect("failed");
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "main",
            &main_tree,
            &[&base_commit],
        )
        .expect("failed");

        repo.branch("feature", &base_commit, false).expect("failed");

        let feat_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob = repo.blob(b"feature").expect("failed");
            builder.insert("file.txt", blob, 0o100644).expect("failed");
            builder.write().expect("failed")
        };
        let feat_tree = repo.find_tree(feat_tree).expect("failed");
        repo.commit(
            Some("refs/heads/feature"),
            &sig,
            &sig,
            "feature",
            &feat_tree,
            &[&base_commit],
        )
        .expect("failed");

        let ops = IntegrationOps::new(&repo);
        let is_clean = ops
            .test_merge_clean("feature", "main")
            .expect("test should work");

        assert!(!is_clean);
    }
}
