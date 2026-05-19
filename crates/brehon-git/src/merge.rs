//! Merge operations.

use git2::{Repository, Signature};
use tracing::{debug, warn};

use crate::error::GitError;
use brehon_ports::MergeResult;

/// Merge operations.
pub struct MergeOps<'a> {
    repo: &'a Repository,
}

impl<'a> MergeOps<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }

    /// Merge a branch into the current branch.
    ///
    /// Returns `MergeResult::Success` if the merge completes cleanly,
    /// or `MergeResult::Conflict` with the conflicting files if conflicts occur.
    ///
    /// If a conflict occurs, the merge is aborted and the current branch is unchanged.
    pub fn merge_branch(&self, branch: &str) -> Result<MergeResult, GitError> {
        debug!("Merging '{}' into current branch", branch);

        let branch_ref = format!("refs/heads/{branch}");
        let their_commit = self.repo.find_reference(&branch_ref)?.peel_to_commit()?;

        let annotation = self.repo.find_annotated_commit(their_commit.id())?;

        let head = self.repo.head()?;
        let _our_commit = head.peel_to_commit()?;

        let analysis = self.repo.merge_analysis(&[&annotation])?;

        if analysis.0.is_up_to_date() {
            debug!("Branch is already up to date");
            return Ok(MergeResult::Success);
        }

        if analysis.0.is_fast_forward() {
            return self.fast_forward_merge(branch, &annotation);
        }

        if analysis.0.is_normal() {
            return self.normal_merge(branch, &annotation, &their_commit);
        }

        Err(GitError::GitOperationFailed(
            "unexpected merge analysis result".into(),
        ))
    }

    /// Perform a fast-forward merge.
    fn fast_forward_merge(
        &self,
        branch: &str,
        annotated_commit: &git2::AnnotatedCommit,
    ) -> Result<MergeResult, GitError> {
        debug!("Performing fast-forward merge from '{}'", branch);

        let target_commit = self.repo.find_commit(annotated_commit.id())?;
        let target_tree = target_commit.tree()?;

        self.repo.checkout_tree(target_tree.as_object(), None)?;

        let head = self.repo.head()?;
        let ref_name = head
            .name()
            .ok_or_else(|| GitError::GitOperationFailed("no HEAD reference name".into()))?;
        let ref_name_owned = ref_name.to_string();
        let mut head_ref = self.repo.find_reference(&ref_name_owned)?;

        head_ref.set_target(annotated_commit.id(), "fast-forward merge")?;

        debug!("Successfully completed fast-forward merge");
        Ok(MergeResult::Success)
    }

    /// Perform a normal (3-way) merge.
    fn normal_merge(
        &self,
        branch: &str,
        annotated_commit: &git2::AnnotatedCommit,
        their_commit: &git2::Commit,
    ) -> Result<MergeResult, GitError> {
        debug!("Performing normal merge from '{}'", branch);

        self.repo.merge(&[annotated_commit], None, None)?;

        let index = self.repo.index()?;

        if index.has_conflicts() {
            let conflict_files = self.get_conflict_files(&index)?;
            warn!("Merge conflict in files: {:?}", conflict_files);

            self.repo.cleanup_state()?;

            return Ok(MergeResult::Conflict {
                files: conflict_files,
            });
        }

        let sig = self.get_signature()?;
        let tree_id = {
            let mut index = self.repo.index()?;
            index.write_tree()?
        };

        let tree = self.repo.find_tree(tree_id)?;

        let head = self.repo.head()?;
        let parent_commit = head.peel_to_commit()?;

        let _merge_commit = self.repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            &format!(
                "Merge branch '{}' into {}",
                branch,
                self.current_branch_name()?
            ),
            &tree,
            &[&parent_commit, their_commit],
        );

        debug!("Successfully completed merge");
        Ok(MergeResult::Success)
    }

    /// Check if a merge would have conflicts without actually performing it.
    pub fn preview_merge_conflicts(&self, branch: &str) -> Result<Vec<String>, GitError> {
        debug!("Previewing merge from '{}'", branch);

        let branch_ref = format!("refs/heads/{branch}");
        let their_commit = self.repo.find_reference(&branch_ref)?.peel_to_commit()?;

        let head = self.repo.head()?;
        let our_commit = head.peel_to_commit()?;

        let merge_base = self.repo.merge_base(our_commit.id(), their_commit.id())?;

        let conflicts =
            self.get_potential_conflicts(our_commit.id(), their_commit.id(), merge_base)?;

        Ok(conflicts)
    }

    /// Get conflicts from merge base.
    pub fn merge_base(&self, branch: &str) -> Result<git2::Oid, GitError> {
        let branch_ref = format!("refs/heads/{branch}");
        let their_commit = self.repo.find_reference(&branch_ref)?.peel_to_commit()?;

        let head = self.repo.head()?;
        let our_commit = head.peel_to_commit()?;

        self.repo
            .merge_base(our_commit.id(), their_commit.id())
            .map_err(Into::into)
    }

    fn get_signature(&self) -> Result<Signature<'a>, GitError> {
        self.repo.signature().or_else(|_| {
            Signature::now("Brehon", "brehon@brehon.ai").map_err(|e| {
                GitError::GitOperationFailed(format!("failed to create signature: {}", e))
            })
        })
    }

    fn current_branch_name(&self) -> Result<String, GitError> {
        let head = self.repo.head()?;
        head.shorthand()
            .map(String::from)
            .ok_or_else(|| GitError::GitOperationFailed("no branch name".into()))
    }

    fn get_conflict_files(&self, index: &git2::Index) -> Result<Vec<String>, GitError> {
        let mut conflicts = Vec::new();

        for conflict in index.conflicts()?.flatten() {
            if let Some(their) = conflict.their {
                conflicts.push(String::from_utf8_lossy(&their.path).to_string());
            } else if let Some(our) = conflict.our {
                conflicts.push(String::from_utf8_lossy(&our.path).to_string());
            }
        }

        conflicts.sort();
        conflicts.dedup();
        Ok(conflicts)
    }

    fn get_potential_conflicts(
        &self,
        our: git2::Oid,
        their: git2::Oid,
        base: git2::Oid,
    ) -> Result<Vec<String>, GitError> {
        let our_commit = self.repo.find_commit(our)?;
        let their_commit = self.repo.find_commit(their)?;
        let base_commit = self.repo.find_commit(base)?;

        let mut our_paths = std::collections::HashSet::new();
        let mut their_paths = std::collections::HashSet::new();

        let base_tree = base_commit.tree()?;
        let our_tree = our_commit.tree()?;
        let their_tree = their_commit.tree()?;

        let our_diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&our_tree), None)?;
        let their_diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&their_tree), None)?;

        our_diff.foreach(
            &mut |delta, _| {
                if let Some(path) = delta.new_file().path() {
                    our_paths.insert(path.to_string_lossy().to_string());
                } else if let Some(path) = delta.old_file().path() {
                    our_paths.insert(path.to_string_lossy().to_string());
                }
                true
            },
            None,
            None,
            None,
        )?;

        their_diff.foreach(
            &mut |delta, _| {
                if let Some(path) = delta.new_file().path() {
                    their_paths.insert(path.to_string_lossy().to_string());
                } else if let Some(path) = delta.old_file().path() {
                    their_paths.insert(path.to_string_lossy().to_string());
                }
                true
            },
            None,
            None,
            None,
        )?;

        let conflicts: Vec<String> = our_paths.intersection(&their_paths).cloned().collect();

        Ok(conflicts)
    }

    /// Abort an in-progress merge (if any).
    pub fn abort_merge(&self) -> Result<(), GitError> {
        debug!("Aborting any in-progress merge");

        let state = self.repo.state();
        if state == git2::RepositoryState::Merge {
            self.repo.cleanup_state()?;
            debug!("Merge aborted successfully");
        }

        Ok(())
    }

    /// Check if there's a merge in progress.
    pub fn is_merge_in_progress(&self) -> bool {
        matches!(self.repo.state(), git2::RepositoryState::Merge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, Repository) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let repo = Repository::init(temp_dir.path()).expect("failed to init repo");
        (temp_dir, repo)
    }

    fn init_main_branch(repo: &Repository) -> git2::Oid {
        let sig = Signature::now("Test", "test@example.com").expect("failed to create sig");
        let mut index = repo.index().expect("failed to get index");
        let oid = index.write_tree().expect("failed to write tree");
        let tree = repo.find_tree(oid).expect("failed to find tree");
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

    #[test]
    fn is_merge_in_progress_false_initially() {
        let (_temp_dir, repo) = create_test_repo();
        init_main_branch(&repo);

        let ops = MergeOps::new(&repo);
        assert!(!ops.is_merge_in_progress());
    }

    #[test]
    fn merge_up_to_date_branch() {
        let (_temp_dir, repo) = create_test_repo();
        let commit = init_main_branch(&repo);

        repo.branch(
            "feature",
            &repo.find_commit(commit).expect("failed to get commit"),
            false,
        )
        .expect("failed to create branch");

        let ops = MergeOps::new(&repo);
        let result = ops.merge_branch("feature").expect("merge should succeed");
        assert!(matches!(result, MergeResult::Success));
    }

    #[test]
    fn abort_merge_clean_state() {
        let (_temp_dir, repo) = create_test_repo();
        init_main_branch(&repo);

        let ops = MergeOps::new(&repo);
        ops.abort_merge()
            .expect("abort should succeed on clean repo");
    }

    #[test]
    fn preview_merge_conflicts_detects_overlapping_changes() {
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
        let tree = repo.find_tree(base_tree).expect("failed to find tree");
        let base_commit = repo
            .commit(None, &sig, &sig, "base", &tree, &[])
            .expect("failed to commit");
        repo.reference("refs/heads/main", base_commit, true, "create main branch")
            .expect("failed");
        repo.set_head("refs/heads/main")
            .expect("failed to set HEAD");
        let base_commit_obj = repo.find_commit(base_commit).expect("failed");

        let main_tree = {
            let mut builder = repo
                .treebuilder(None)
                .expect("failed to create tree builder");
            let blob_id = repo.blob(b"main changed").expect("failed to create blob");
            builder
                .insert("file.txt", blob_id, 0o100644)
                .expect("failed to insert");
            builder.write().expect("failed to write tree")
        };
        let main_tree_obj = repo.find_tree(main_tree).expect("failed to find tree");
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

        repo.branch("feature", &base_commit_obj, false)
            .expect("failed");

        let feat_tree = {
            let mut builder = repo
                .treebuilder(None)
                .expect("failed to create tree builder");
            let blob_id = repo
                .blob(b"feature changed")
                .expect("failed to create blob");
            builder
                .insert("file.txt", blob_id, 0o100644)
                .expect("failed to insert");
            builder.write().expect("failed to write tree")
        };
        let feat_tree_obj = repo.find_tree(feat_tree).expect("failed to find tree");
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
        repo.reference(
            "refs/heads/feature",
            feat_commit,
            true,
            "create feature branch",
        )
        .expect("failed");

        let ops = MergeOps::new(&repo);
        let conflicts = ops
            .preview_merge_conflicts("feature")
            .expect("preview should work");
        assert!(conflicts.contains(&"file.txt".to_string()));
    }
}
