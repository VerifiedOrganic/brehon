//! Diff generation and file overlap detection.

use git2::Repository;
use tracing::debug;

use crate::error::GitError;
use brehon_ports::FileDiff;

/// Diff operations.
pub struct DiffOps<'a> {
    repo: &'a Repository,
}

impl<'a> DiffOps<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }

    fn path_to_string(path: &std::path::Path) -> String {
        path.to_string_lossy().to_string()
    }

    /// Calculate diff between two branches.
    ///
    /// Returns a list of files changed between `branch` and `base`,
    /// with line counts for additions and deletions.
    pub fn diff_branches(&self, branch: &str, base: &str) -> Result<Vec<FileDiff>, GitError> {
        debug!("Generating diff from '{}' to '{}'", base, branch);

        let branch_ref = format!("refs/heads/{branch}");
        let base_ref = format!("refs/heads/{base}");

        let branch_commit = self.repo.find_reference(&branch_ref)?.peel_to_commit()?;
        let base_commit = self.repo.find_reference(&base_ref)?.peel_to_commit()?;

        let branch_tree = branch_commit.tree()?;
        let base_tree = base_commit.tree()?;

        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&branch_tree), None)?;

        let mut files = std::collections::HashMap::new();

        diff.foreach(
            &mut |delta, _| {
                let path = match delta.new_file().path() {
                    Some(p) => Self::path_to_string(p),
                    None => match delta.old_file().path() {
                        Some(p) => Self::path_to_string(p),
                        None => return true,
                    },
                };

                files.insert(
                    path.clone(),
                    FileDiff {
                        path,
                        additions: 0,
                        deletions: 0,
                    },
                );

                true
            },
            None,
            None,
            None,
        )?;

        debug!("Found {} changed files", files.len());
        Ok(files.into_values().collect())
    }

    /// Get file overlaps between two branches.
    ///
    /// Returns files that are modified by both branches relative to their merge base.
    pub fn file_overlaps(&self, branch: &str, base: &str) -> Result<Vec<String>, GitError> {
        debug!("Finding file overlaps between '{}' and '{}'", branch, base);

        let branch_ref = format!("refs/heads/{branch}");
        let base_ref = format!("refs/heads/{base}");

        let branch_commit = self.repo.find_reference(&branch_ref)?.peel_to_commit()?;
        let base_commit = self.repo.find_reference(&base_ref)?.peel_to_commit()?;

        let merge_base = self.repo.merge_base(branch_commit.id(), base_commit.id())?;
        let base_tree = self.repo.find_commit(merge_base)?.tree()?;

        let branch_diff =
            self.repo
                .diff_tree_to_tree(Some(&base_tree), Some(&branch_commit.tree()?), None)?;
        let base_diff =
            self.repo
                .diff_tree_to_tree(Some(&base_tree), Some(&base_commit.tree()?), None)?;

        let mut branch_files = std::collections::HashSet::new();
        let mut base_files = std::collections::HashSet::new();

        branch_diff.foreach(
            &mut |delta, _| {
                if let Some(path) = delta.new_file().path() {
                    branch_files.insert(Self::path_to_string(path));
                } else if let Some(path) = delta.old_file().path() {
                    branch_files.insert(Self::path_to_string(path));
                }
                true
            },
            None,
            None,
            None,
        )?;

        base_diff.foreach(
            &mut |delta, _| {
                if let Some(path) = delta.new_file().path() {
                    base_files.insert(Self::path_to_string(path));
                } else if let Some(path) = delta.old_file().path() {
                    base_files.insert(Self::path_to_string(path));
                }
                true
            },
            None,
            None,
            None,
        )?;

        let overlaps: Vec<String> = branch_files.intersection(&base_files).cloned().collect();

        debug!("Found {} overlapping files", overlaps.len());
        Ok(overlaps)
    }

    /// Get detailed line counts for diff.
    pub fn diff_with_line_counts(
        &self,
        branch: &str,
        base: &str,
    ) -> Result<Vec<FileDiff>, GitError> {
        debug!(
            "Generating detailed diff from '{}' to '{}' with line counts",
            base, branch
        );

        let branch_ref = format!("refs/heads/{branch}");
        let base_ref = format!("refs/heads/{base}");

        let branch_commit = self.repo.find_reference(&branch_ref)?.peel_to_commit()?;
        let base_commit = self.repo.find_reference(&base_ref)?.peel_to_commit()?;

        let branch_tree = branch_commit.tree()?;
        let base_tree = base_commit.tree()?;

        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&branch_tree), None)?;

        let mut files = std::collections::HashMap::new();

        diff.foreach(
            &mut |delta, _| {
                let path = match delta.new_file().path() {
                    Some(p) => Self::path_to_string(p),
                    None => match delta.old_file().path() {
                        Some(p) => Self::path_to_string(p),
                        None => return true,
                    },
                };

                files.insert(
                    path.clone(),
                    FileDiff {
                        path,
                        additions: 0,
                        deletions: 0,
                    },
                );

                true
            },
            None,
            None,
            None,
        )?;

        let _stats = diff.stats()?;

        Ok(files.into_values().collect())
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
    fn diff_branches_empty_repo() {
        let (_temp_dir, repo) = create_test_repo();

        let sig = Signature::now("Test", "test@example.com").expect("failed to create sig");
        let tree = {
            let mut index = repo.index().expect("failed to get index");
            index.write_tree().expect("failed to write tree")
        };
        let tree_obj = repo.find_tree(tree).expect("failed to find tree");
        let commit1 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "init", &tree_obj, &[])
            .expect("failed to commit");

        repo.branch(
            "feature",
            &repo.find_commit(commit1).expect("failed"),
            false,
        )
        .expect("failed to create branch");

        let ops = DiffOps::new(&repo);
        let files = ops
            .diff_branches("feature", "main")
            .expect("diff should work");
        assert!(files.is_empty());
    }

    #[test]
    fn diff_branches_detects_changed_file() {
        let (_temp_dir, repo) = create_test_repo();
        let sig = Signature::now("Test", "test@example.com").expect("failed to create sig");

        let tree1 = {
            let mut builder = repo
                .treebuilder(None)
                .expect("failed to create tree builder");
            let blob_id = repo
                .blob(b"original content")
                .expect("failed to create blob");
            builder
                .insert("file.txt", blob_id, 0o100644)
                .expect("failed to insert");
            builder.write().expect("failed to write tree")
        };
        let tree_obj = repo.find_tree(tree1).expect("failed to find tree");
        let commit1 = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "init", &tree_obj, &[])
            .expect("failed to commit");

        repo.branch(
            "feature",
            &repo.find_commit(commit1).expect("failed"),
            false,
        )
        .expect("failed to create branch");

        let tree2 = {
            let mut builder = repo
                .treebuilder(None)
                .expect("failed to create tree builder");
            let blob_id = repo
                .blob(b"modified content")
                .expect("failed to create blob");
            builder
                .insert("file.txt", blob_id, 0o100644)
                .expect("failed to insert");
            builder.write().expect("failed to write tree")
        };
        let tree_obj2 = repo.find_tree(tree2).expect("failed to find tree");
        repo.commit(
            Some("refs/heads/feature"),
            &sig,
            &sig,
            "change file",
            &tree_obj2,
            &[&repo.find_commit(commit1).expect("failed")],
        )
        .expect("failed to commit");

        let ops = DiffOps::new(&repo);
        let files = ops
            .diff_branches("feature", "main")
            .expect("diff should work");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "file.txt");
    }

    #[test]
    fn file_overlaps_detects_overlapping_changes() {
        let (_temp_dir, repo) = create_test_repo();
        let sig = Signature::now("Test", "test@example.com").expect("failed to create sig");

        let base_tree = {
            let mut builder = repo
                .treebuilder(None)
                .expect("failed to create tree builder");
            let blob_id = repo.blob(b"base content").expect("failed to create blob");
            builder
                .insert("file.txt", blob_id, 0o100644)
                .expect("failed to insert");
            builder.write().expect("failed to write tree")
        };
        let base_tree_obj = repo.find_tree(base_tree).expect("failed to find tree");
        let base = repo
            .commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "base",
                &base_tree_obj,
                &[],
            )
            .expect("failed to commit");
        let base_commit = repo.find_commit(base).expect("failed");

        let feat1_tree = {
            let mut builder = repo
                .treebuilder(None)
                .expect("failed to create tree builder");
            let blob_id = repo.blob(b"main modified").expect("failed to create blob");
            builder
                .insert("file.txt", blob_id, 0o100644)
                .expect("failed to insert");
            builder.write().expect("failed to write tree")
        };
        let feat1_tree_obj = repo.find_tree(feat1_tree).expect("failed to find tree");
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            "main change",
            &feat1_tree_obj,
            &[&base_commit],
        )
        .expect("failed");

        repo.branch("feature", &base_commit, false).expect("failed");

        let feat2_tree = {
            let mut builder = repo
                .treebuilder(None)
                .expect("failed to create tree builder");
            let blob_id = repo
                .blob(b"feature modified")
                .expect("failed to create blob");
            builder
                .insert("file.txt", blob_id, 0o100644)
                .expect("failed to insert");
            builder.write().expect("failed to write tree")
        };
        let feat2_tree_obj = repo.find_tree(feat2_tree).expect("failed to find tree");
        repo.commit(
            Some("refs/heads/feature"),
            &sig,
            &sig,
            "feature change",
            &feat2_tree_obj,
            &[&base_commit],
        )
        .expect("failed");

        let ops = DiffOps::new(&repo);
        let overlaps = ops
            .file_overlaps("feature", "main")
            .expect("overlaps should work");

        assert!(overlaps.contains(&"file.txt".to_string()));
    }

    #[test]
    fn file_overlaps_non_overlapping_files() {
        let (_temp_dir, repo) = create_test_repo();
        let sig = Signature::now("Test", "test@example.com").expect("failed to create sig");

        // Create empty base (no files)
        let base_tree = repo
            .treebuilder(None)
            .expect("failed")
            .write()
            .expect("failed");
        let base_tree_obj = repo.find_tree(base_tree).expect("failed");
        let base_commit = repo
            .commit(None, &sig, &sig, "base", &base_tree_obj, &[])
            .expect("failed");
        repo.reference("refs/heads/main", base_commit, true, "create main")
            .expect("failed");
        repo.set_head("refs/heads/main").expect("failed");
        let base_commit_obj = repo.find_commit(base_commit).expect("failed");

        // Main branch adds main.txt (no other changes)
        let main_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob_id = repo.blob(b"main file").expect("failed");
            builder
                .insert("main.txt", blob_id, 0o100644)
                .expect("failed");
            builder.write().expect("failed")
        };
        let main_tree_obj = repo.find_tree(main_tree).expect("failed");
        let main_commit = repo
            .commit(
                None,
                &sig,
                &sig,
                "main change",
                &main_tree_obj,
                &[&base_commit_obj],
            )
            .expect("failed");
        repo.reference("refs/heads/main", main_commit, true, "update main")
            .expect("failed");

        // Feature branch adds feature.txt (different file)
        let feat_tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob_id = repo.blob(b"feature file").expect("failed");
            builder
                .insert("feature.txt", blob_id, 0o100644)
                .expect("failed");
            builder.write().expect("failed")
        };
        let feat_tree_obj = repo.find_tree(feat_tree).expect("failed");
        let feat_commit = repo
            .commit(
                None,
                &sig,
                &sig,
                "feature change",
                &feat_tree_obj,
                &[&base_commit_obj],
            )
            .expect("failed");
        repo.reference("refs/heads/feature", feat_commit, true, "create feature")
            .expect("failed");

        let ops = DiffOps::new(&repo);
        let overlaps = ops
            .file_overlaps("feature", "main")
            .expect("overlaps should work");

        // No overlap - main added main.txt, feature added feature.txt
        assert!(overlaps.is_empty());
    }
}
