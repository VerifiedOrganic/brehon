use std::path::Path;

use super::{git_commit_is_ancestor_in, git_stdout_in};

/// Check whether the tree changes introduced by `sha` are already present
/// on `branch` in `wt`.
///
/// This compares the blob contents at every path changed by `sha`. A commit
/// message trailer or a path-only diff is not sufficient proof that the branch
/// contains the reviewed content.
pub(crate) fn tree_matches_after(wt: &Path, sha: &str, branch: &str) -> Result<bool, String> {
    Ok(tree_matches_after_with_limit(wt, sha, branch, None)?.unwrap_or(false))
}

/// Bounded variant of [`tree_matches_after`].
///
/// Returns `Ok(None)` when the reviewed commit touches more than
/// `max_changed_files`, so callers can use this as a guarded fallback in
/// retry-state probes without scanning huge cache-cleanup commits.
pub(crate) fn tree_matches_after_limited(
    wt: &Path,
    sha: &str,
    branch: &str,
    max_changed_files: usize,
) -> Result<Option<bool>, String> {
    tree_matches_after_with_limit(wt, sha, branch, Some(max_changed_files))
}

fn tree_matches_after_with_limit(
    wt: &Path,
    sha: &str,
    branch: &str,
    max_changed_files: Option<usize>,
) -> Result<Option<bool>, String> {
    if git_commit_is_ancestor_in(wt, sha, branch).unwrap_or(false) {
        return Ok(Some(true));
    }

    let parent_commit = match git_stdout_in(wt, &["rev-parse", &format!("{sha}^")]) {
        Ok(commit) => commit,
        Err(_) => return Ok(Some(false)),
    };

    let diff_expected = crate::git_exec::run_git(
        wt,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            &parent_commit,
            sha,
        ],
    )?;

    if !diff_expected.status.success() {
        return Ok(Some(false));
    }

    let expected_files: Vec<String> = String::from_utf8_lossy(&diff_expected.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    if expected_files.is_empty() {
        return Ok(Some(true));
    }

    if let Some(limit) = max_changed_files {
        if expected_files.len() > limit {
            return Ok(None);
        }
    }

    let mut exact_match = true;
    let mut content_present = true;
    for file in &expected_files {
        let expected = blob_bytes_at(wt, sha, file)?;
        let actual = blob_bytes_at(wt, branch, file)?;
        if expected != actual {
            exact_match = false;
            content_present &= match (expected.as_deref(), actual.as_deref()) {
                (Some(expected), Some(actual)) => bytes_contains(actual, expected),
                (None, None) => true,
                _ => false,
            };
        }
    }

    if exact_match {
        return Ok(Some(true));
    }

    let branch_head = git_stdout_in(wt, &["rev-parse", branch])?;
    let worktree_head = git_stdout_in(wt, &["rev-parse", "HEAD"])?;
    if branch_head == worktree_head && reviewed_patch_reverses_cleanly(wt, &parent_commit, sha)? {
        return Ok(Some(true));
    }

    Ok(Some(content_present))
}

fn blob_bytes_at(wt: &Path, rev: &str, file: &str) -> Result<Option<Vec<u8>>, String> {
    let spec = format!("{rev}:{file}");
    let exists = crate::git_exec::run_git(wt, &["cat-file", "-e", &spec])?;
    if !exists.status.success() {
        return Ok(None);
    }

    let output = crate::git_exec::run_git(wt, &["show", &spec])?;
    if !output.status.success() {
        return Err(format!(
            "Failed to read blob {spec}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(Some(output.stdout))
}

fn reviewed_patch_reverses_cleanly(wt: &Path, parent: &str, sha: &str) -> Result<bool, String> {
    let patch = crate::git_exec::run_git(wt, &["diff", "--binary", parent, sha])?;
    if !patch.status.success() {
        return Ok(false);
    }
    if patch.stdout.is_empty() {
        return Ok(true);
    }

    let output = crate::git_exec::run_git_with_stdin(
        wt,
        &["apply", "--check", "--reverse", "-"],
        &patch.stdout,
    )?;
    Ok(output.status.success())
}

fn bytes_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
