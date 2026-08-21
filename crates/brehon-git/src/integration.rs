//! Integration worktree operations for preflight checks.

use std::process::Command;

use git2::Repository;
use tracing::debug;

use crate::error::GitError;
use crate::rebase::RebaseOps;

/// Extract the conflicted paths from `git merge-tree --write-tree --name-only -z`.
///
/// The NUL-separated layout is: result tree OID, then one field per conflicted
/// path, then an empty field terminating that section, then informational
/// messages we do not need.
fn parse_merge_tree_conflicts(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .split('\0')
        .skip(1)
        .take_while(|field| !field.is_empty())
        .map(str::to_string)
        .collect()
}

/// Integration operations.
pub struct IntegrationOps<'a> {
    repo: &'a Repository,
}

impl<'a> IntegrationOps<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }

    /// Preview merge conflicts without modifying anything.
    ///
    /// Returns the paths that would conflict if `branch` were merged into
    /// `base`.
    ///
    /// Shells out to `git merge-tree --write-tree`, which runs the same ORT
    /// strategy `git merge` will — fidelity to what the real merge does is the
    /// entire point of a preflight check. `git2::Repository::merge_commits` is
    /// deliberately not used: libgit2's merge is not ORT and has no
    /// directory-rename detection, so it answers a different question.
    pub fn preview_conflicts(&self, branch: &str, base: &str) -> Result<Vec<String>, GitError> {
        debug!("Previewing conflicts between '{}' and '{}'", branch, base);

        let cwd = self.repo.workdir().unwrap_or_else(|| self.repo.path());
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                base,
                branch,
            ])
            .output()
            .map_err(|err| {
                GitError::GitOperationFailed(format!(
                    "failed to run git merge-tree for '{branch}' into '{base}': {err}"
                ))
            })?;

        // Exit 1 means "conflicts" *or* "bad input" — an unknown ref exits 1
        // with empty stdout, so the code alone cannot tell them apart. On a
        // real conflict stdout always carries at least the result tree OID.
        match output.status.code() {
            Some(0) => Ok(Vec::new()),
            Some(1) if !output.stdout.is_empty() => Ok(parse_merge_tree_conflicts(&output.stdout)),
            _ => Err(GitError::GitOperationFailed(format!(
                "git merge-tree failed for '{branch}' into '{base}': {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    /// Whether merging `branch` into `base` would be conflict-free.
    ///
    /// Nothing is written and no worktree is created; this is
    /// [`Self::preview_conflicts`] reduced to a yes/no.
    pub fn test_merge_clean(&self, branch: &str, base: &str) -> Result<bool, GitError> {
        debug!("Testing merge of '{}' into '{}' in isolation", branch, base);

        let conflicts = self.preview_conflicts(branch, base)?;
        Ok(conflicts.is_empty())
    }

    /// Whether rebasing `branch` onto `onto` would be conflict-free.
    ///
    /// Nothing is written and no worktree is created; this delegates to
    /// [`RebaseOps::preview_rebase_conflicts`].
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

    /// Commit `content` as `file.txt` on `branch`, parented on `parent`.
    fn commit_file(
        repo: &Repository,
        branch: &str,
        content: &[u8],
        parent: Option<git2::Oid>,
    ) -> git2::Oid {
        let sig = Signature::now("Test", "test@example.com").expect("failed");
        let tree = {
            let mut builder = repo.treebuilder(None).expect("failed");
            let blob = repo.blob(content).expect("failed");
            builder.insert("file.txt", blob, 0o100644).expect("failed");
            repo.find_tree(builder.write().expect("failed"))
                .expect("failed")
        };
        let parents: Vec<git2::Commit> = parent
            .map(|oid| repo.find_commit(oid).expect("failed"))
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &sig,
            &sig,
            branch,
            &tree,
            &parent_refs,
        )
        .expect("failed")
    }

    /// T4: the old implementation intersected changed-file sets, so two
    /// branches that both touched a file — anywhere in it — were reported as
    /// conflicting. On real project branches that produced 215 false
    /// conflicts where `git merge-tree` reported 0.
    #[test]
    fn preview_conflicts_ignores_non_overlapping_edits_to_the_same_file() {
        let (_temp_dir, repo) = create_test_repo();
        let base = commit_file(&repo, "main", b"line1\nline2\nline3\nline4\nline5\n", None);
        commit_file(
            &repo,
            "main",
            b"MAIN\nline2\nline3\nline4\nline5\n",
            Some(base),
        );
        repo.branch("feature", &repo.find_commit(base).expect("failed"), false)
            .expect("failed");
        commit_file(
            &repo,
            "feature",
            b"line1\nline2\nline3\nline4\nFEATURE\n",
            Some(base),
        );

        let ops = IntegrationOps::new(&repo);
        let conflicts = ops
            .preview_conflicts("feature", "main")
            .expect("preview should work");

        assert!(
            conflicts.is_empty(),
            "both branches touched file.txt but in disjoint hunks, so git merges \
             them cleanly; reported: {conflicts:?}"
        );
    }

    /// A bad ref exits 1 with empty stdout, exactly as a real conflict exits 1
    /// — so the exit code alone cannot distinguish them.
    #[test]
    fn preview_conflicts_rejects_an_unknown_ref() {
        let (_temp_dir, repo) = create_test_repo();
        commit_file(&repo, "main", b"seed\n", None);

        let ops = IntegrationOps::new(&repo);
        let err = ops
            .preview_conflicts("no-such-branch", "main")
            .expect_err("an unknown ref must not be reported as a clean merge");

        assert!(
            !matches!(err, GitError::MergeConflict(_)),
            "invalid input must not masquerade as a conflict: {err}"
        );
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
