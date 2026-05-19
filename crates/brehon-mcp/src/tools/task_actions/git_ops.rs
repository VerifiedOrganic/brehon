//! Git operations helpers: branch checks, merge-base, commit ancestry, status, worktree utilities.

use std::path::{Path, PathBuf};

use super::paths::{project_root, workspace_root};

pub(super) fn git_stdout_in(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = crate::git_exec::run_git(cwd, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!(
                "git {} exited with status {}",
                args.join(" "),
                output.status
            )
        } else {
            stderr
        };
        return Err(detail);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err(format!("git {} returned empty output", args.join(" ")));
    }
    Ok(stdout)
}

pub(super) fn git_stdout(args: &[&str]) -> Result<String, String> {
    let root = workspace_root()
        .ok_or_else(|| "No workspace root available for git merge verification.".to_string())?;
    git_stdout_in(&root, args)
}

pub(super) fn current_git_head() -> Option<String> {
    git_stdout(&["rev-parse", "HEAD"]).ok()
}

pub(super) fn current_git_branch() -> Option<String> {
    git_stdout(&["branch", "--show-current"]).ok()
}

pub(super) fn expected_worktree_branch() -> Option<String> {
    std::env::var("BREHON_WORKTREE_BRANCH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(super) fn ensure_worker_branch_safe_for_task(
    task_id: &str,
    task_data: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let Some(current_branch) = current_git_branch() else {
        return Ok(());
    };

    let merge_target = task_data
        .get("merge_target")
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let expected_branch = expected_worktree_branch();

    if let Some(ref expected_branch) = expected_branch {
        if &current_branch != expected_branch {
            let mut detail = format!(
                "Worker branch mismatch for task {task_id}: current branch is '{current_branch}', \
                 but this pane was launched on '{expected_branch}'."
            );
            if merge_target.as_deref() == Some(current_branch.as_str()) {
                detail.push_str(&format!(
                    " '{current_branch}' is the task's merge_target/integration branch; \
                     workers must stay on their dedicated branch and let supervisor/epic integration land work there."
                ));
            } else {
                detail.push_str(
                    " Workers must stay on their dedicated branch/worktree instead of checking out other refs in place.",
                );
            }
            return Err(detail);
        }
    }

    if let Some(ref merge_target) = merge_target {
        if &current_branch == merge_target {
            return Err(format!(
                "Worker cannot report progress for task {task_id} while checked out on merge_target '{merge_target}'. \
                 Stay on the dedicated worker branch and merge/rebase '{merge_target}' into it if you need the latest base."
            ));
        }
    }

    let default_branch = detect_default_branch().unwrap_or_else(|_| "main".to_string());
    if current_branch == default_branch {
        return Err(format!(
            "Worker cannot report progress for task {task_id} while checked out on default branch '{default_branch}'. \
             Workers must not work directly on the landing branch."
        ));
    }

    Ok(())
}

pub(super) fn git_commit_is_ancestor(commit: &str, reference: &str) -> Result<bool, String> {
    let root = workspace_root()
        .ok_or_else(|| "No workspace root available for git merge verification.".to_string())?;
    git_commit_is_ancestor_in(&root, commit, reference)
}

pub(super) fn git_commit_is_ancestor_in(
    cwd: &Path,
    commit: &str,
    reference: &str,
) -> Result<bool, String> {
    let output =
        crate::git_exec::run_git(cwd, &["merge-base", "--is-ancestor", commit, reference])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                format!(
                    "git merge-base --is-ancestor exited with status {}",
                    output.status
                )
            } else {
                stderr
            })
        }
    }
}

pub(super) fn git_branch_exists_in(cwd: &Path, branch: &str) -> Result<bool, String> {
    let ref_path = format!("refs/heads/{branch}");
    let output = crate::git_exec::run_git(cwd, &["rev-parse", "--verify", &ref_path])?;
    Ok(output.status.success())
}

pub(super) fn git_run_ok_in(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = crate::git_exec::run_git(cwd, args)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!(
            "git {} exited with status {}",
            args.join(" "),
            output.status
        )
    } else {
        stderr
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MergeStatus {
    MergedLocally,
    MergedRemotely,
    RemoteStatusUnknown,
}

impl MergeStatus {
    pub(super) fn display(&self) -> &'static str {
        match self {
            MergeStatus::MergedLocally => "merged locally, not yet on remote",
            MergeStatus::MergedRemotely => "merged locally and remotely",
            MergeStatus::RemoteStatusUnknown => "merged locally, remote status unknown",
        }
    }
}

pub(super) fn git_status_porcelain_in(cwd: &Path) -> Result<String, String> {
    let output = crate::git_exec::run_git(cwd, &["status", "--porcelain"])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git status --porcelain exited with status {}",
                output.status
            )
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(super) fn non_brehon_status_entries(status: &str) -> Vec<&str> {
    status
        .lines()
        .filter(|line| {
            let path = line.get(3..).unwrap_or("").trim();
            !path.is_empty() && !path.starts_with(".brehon/")
        })
        .collect()
}

fn explicit_project_root() -> Option<PathBuf> {
    std::env::var("BREHON_PROJECT_ROOT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn shared_root_escape_status_entries(status: &str) -> Vec<&str> {
    status
        .lines()
        .filter(|line| {
            let path = line.get(3..).unwrap_or("").trim();
            !path.is_empty()
                && !path.starts_with(".brehon/")
                && path != ".mcp.json"
                && path != ".claude/settings.local.json"
        })
        .collect()
}

fn ensure_shared_root_clean_for_checkpoint(cwd: &Path) -> Result<(), String> {
    let Some(project) = explicit_project_root() else {
        return Ok(());
    };
    let (Ok(canonical_project), Ok(canonical_cwd)) = (project.canonicalize(), cwd.canonicalize())
    else {
        return Ok(());
    };
    if canonical_project == canonical_cwd {
        return Ok(());
    }
    if !canonical_project.join(".git").exists() {
        return Ok(());
    }

    let status = git_status_porcelain_in(&canonical_project).map_err(|err| {
        format!(
            "Failed to inspect shared project checkout '{}' before checkpointing worker worktree '{}': {err}",
            canonical_project.display(),
            cwd.display()
        )
    })?;
    let entries = shared_root_escape_status_entries(&status);
    if entries.is_empty() {
        return Ok(());
    }

    let preview = entries
        .iter()
        .take(8)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let extra = entries.len().saturating_sub(8);
    let suffix = if extra == 0 {
        String::new()
    } else {
        format!(", ... plus {extra} more")
    };
    Err(format!(
        "Refusing to checkpoint worker worktree '{}' because the shared project checkout '{}' has local changes: {preview}{suffix}. \
         This usually means the agent edited files outside its assigned worktree. Move those edits into the worker worktree or clean the shared checkout before completing; otherwise Brehon would record the worker's existing HEAD and create an empty or incomplete review.",
        cwd.display(),
        canonical_project.display()
    ))
}

pub(super) fn current_workspace_root() -> Result<PathBuf, String> {
    std::env::var("BREHON_WORKSPACE_ROOT")
        .ok()
        .and_then(|root| {
            let root = root.trim();
            (!root.is_empty()).then(|| PathBuf::from(root))
        })
        .ok_or_else(|| {
            "No BREHON_WORKSPACE_ROOT available. This action must run from a worker worktree."
                .to_string()
        })
}

pub(super) fn commit_workspace_checkpoint(
    cwd: &Path,
    message: &str,
) -> Result<(String, bool), String> {
    let message = message.trim();
    if message.is_empty() {
        return Err("Checkpoint message must be non-empty.".to_string());
    }

    ensure_checkpoint_cwd_is_isolated(cwd)?;
    ensure_shared_root_clean_for_checkpoint(cwd)?;

    let status_before = git_status_porcelain_in(cwd)?;
    let had_changes = !status_before.trim().is_empty();
    if !had_changes {
        let head = git_stdout_in(cwd, &["rev-parse", "HEAD"])?;
        return Ok((head, false));
    }

    git_run_ok_in(cwd, &["add", "-A"])
        .map_err(|err| format!("Failed to stage workspace changes for checkpoint: {err}"))?;
    git_run_ok_in(cwd, &["commit", "-m", message])
        .map_err(|err| format!("Failed to commit workspace checkpoint: {err}"))?;
    let head = git_stdout_in(cwd, &["rev-parse", "HEAD"])?;
    Ok((head, true))
}

/// Refuses a checkpoint `cwd` that would mutate the shared project checkout
/// or the project's default branch. Worker checkpoints must always run inside
/// an isolated worktree on a dedicated branch; if `BREHON_WORKSPACE_ROOT` is
/// ever pointed at the primary checkout (or a worker is somehow checked out
/// on `main`), `git add -A` would silently stage the wrong tree into the
/// shared index — see the post-mortem in worker.rs for the failure mode this
/// guards against.
pub(super) fn ensure_checkpoint_cwd_is_isolated(cwd: &Path) -> Result<(), String> {
    // Path-equality check: only fires when BREHON_PROJECT_ROOT is explicitly
    // set (which the brehon worker spawn paths always do — see
    // brehon-cli/src/commands/task.rs and brehon-adapter-sdk). This is the
    // primary failure-mode guard: it directly catches a worker whose
    // BREHON_WORKSPACE_ROOT was misconfigured to the same path as
    // BREHON_PROJECT_ROOT.
    if std::env::var("BREHON_PROJECT_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .is_some()
    {
        if let (Some(project), Ok(canonical_cwd)) = (
            project_root().and_then(|p| p.canonicalize().ok()),
            cwd.canonicalize(),
        ) {
            if canonical_cwd == project {
                return Err(format!(
                    "Refusing to checkpoint at the primary project checkout '{}'. \
                     Workers must commit from an isolated worktree under '.brehon/worktrees/'; \
                     something pointed BREHON_WORKSPACE_ROOT at the shared repo root.",
                    cwd.display()
                ));
            }
        }
    }

    let current_branch = git_stdout_in(cwd, &["branch", "--show-current"]).ok();
    let default_branch = detect_default_branch_in(cwd).unwrap_or_else(|_| "main".to_string());
    if let Some(branch) = current_branch.as_deref() {
        if branch == default_branch {
            return Err(format!(
                "Refusing to checkpoint while '{}' is checked out on default branch '{}'. \
                 Workers must operate on a dedicated worker branch in an isolated worktree.",
                cwd.display(),
                default_branch
            ));
        }
        if matches!(branch, "main" | "master") {
            return Err(format!(
                "Refusing to checkpoint while '{}' is checked out on '{}'. \
                 Workers must operate on a dedicated worker branch.",
                cwd.display(),
                branch
            ));
        }
    }

    Ok(())
}

pub(super) fn detect_default_branch() -> Result<String, String> {
    match workspace_root() {
        Some(root) => detect_default_branch_in(&root),
        None => Ok("main".to_string()),
    }
}

pub(super) fn detect_default_branch_in(cwd: &Path) -> Result<String, String> {
    if let Ok(branch) = git_stdout_in(cwd, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(stripped) = branch.strip_prefix("refs/remotes/origin/") {
            return Ok(stripped.to_string());
        }
    }

    for candidate in ["main", "master", "develop"] {
        let ref_path = format!("refs/heads/{candidate}");
        if git_stdout_in(cwd, &["rev-parse", "--verify", &ref_path]).is_ok() {
            return Ok(candidate.to_string());
        }
    }

    Ok("main".to_string())
}

fn resolve_git_info_dir(root: &Path) -> Result<PathBuf, String> {
    let git_entry = root.join(".git");
    if git_entry.is_file() {
        let contents = std::fs::read_to_string(&git_entry).map_err(|err| {
            format!(
                "Failed to read .git file at '{}': {err}",
                git_entry.display()
            )
        })?;
        let gitdir = contents
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    ".git file at '{}' does not contain a valid gitdir pointer",
                    git_entry.display()
                )
            })?;
        let gitdir_path = PathBuf::from(gitdir);
        let resolved = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            root.join(gitdir_path)
        };
        let commondir_line = std::fs::read_to_string(resolved.join("commondir"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let info_base = if let Some(ref common) = commondir_line {
            let common_path = PathBuf::from(common);
            let common_resolved = if common_path.is_absolute() {
                common_path
            } else {
                resolved.join(&common_path)
            };
            common_resolved.join("info")
        } else {
            resolved.join("info")
        };
        Ok(info_base)
    } else {
        Ok(git_entry.join("info"))
    }
}

pub(super) fn ensure_brehon_ignored_in_repo(root: &Path) -> Result<(), String> {
    let info_dir = resolve_git_info_dir(root)?;
    let exclude_path = info_dir.join("exclude");
    let parent = exclude_path
        .parent()
        .ok_or_else(|| format!("Invalid git exclude path '{}'.", exclude_path.display()))?;
    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "Failed to create git exclude directory '{}': {err}",
            parent.display()
        )
    })?;

    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing
        .lines()
        .any(|line| matches!(line.trim(), ".brehon/" | ".brehon"))
    {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("# Brehon orchestration data\n.brehon/\n");
    std::fs::write(&exclude_path, updated).map_err(|err| {
        format!(
            "Failed to update git exclude file '{}': {err}",
            exclude_path.display()
        )
    })
}

pub(super) fn detect_remote_merge_status(commit: &str, default_branch: &str) -> MergeStatus {
    if !git_commit_is_ancestor(commit, "HEAD").unwrap_or(false) {
        return MergeStatus::RemoteStatusUnknown;
    }

    let remote_ref = format!("origin/{default_branch}");
    if git_stdout(&["rev-parse", "--verify", &remote_ref]).is_err() {
        return MergeStatus::RemoteStatusUnknown;
    }

    if git_commit_is_ancestor(commit, &remote_ref).unwrap_or(false) {
        MergeStatus::MergedRemotely
    } else {
        MergeStatus::MergedLocally
    }
}

pub(super) fn unmerged_files(cwd: &Path) -> Result<Vec<String>, String> {
    let output = crate::git_exec::run_git(cwd, &["diff", "--name-only", "--diff-filter=U"])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git diff --name-only --diff-filter=U exited with status {}",
                output.status
            )
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Check whether a cherry-pick is in progress by looking for `CHERRY_PICK_HEAD`.
pub(super) fn cherry_pick_in_progress_in(wt: &Path) -> bool {
    cherry_pick_sha_in(wt).is_some()
}

/// Read the SHA from `.git/CHERRY_PICK_HEAD` if it exists.
pub(super) fn cherry_pick_sha_in(wt: &Path) -> Option<String> {
    let git_path = match git_stdout_in(wt, &["rev-parse", "--git-path", "CHERRY_PICK_HEAD"]) {
        Ok(p) => p,
        Err(_) => return None,
    };
    let path = PathBuf::from(&git_path);
    let path = if path.is_absolute() {
        path
    } else {
        wt.join(path)
    };
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Compute the stable patch-id for a commit in a worktree.
pub(super) fn git_patch_id(wt: &Path, sha: &str) -> Result<Option<String>, String> {
    let diff_output = crate::git_exec::run_git(wt, &["diff-tree", "-p", sha])?;
    if !diff_output.status.success() {
        return Err(format!(
            "git diff-tree failed: {}",
            String::from_utf8_lossy(&diff_output.stderr)
        ));
    }

    let output =
        crate::git_exec::run_git_with_stdin(wt, &["patch-id", "--stable"], &diff_output.stdout)?;
    if !output.status.success() {
        return Ok(None);
    }

    let line = String::from_utf8_lossy(&output.stdout);
    let patch_id = line.split_whitespace().next().map(str::to_string);
    Ok(patch_id)
}

/// Walk backwards from `branch` tip up to `window` commits and compare
/// patch-ids to `sha`. Returns `true` on first match.
///
/// Uses `--max-count={window}` rather than `{branch}~{window}..{branch}` so
/// the probe works on branches shorter than `window` commits (e.g. a fresh
/// epic branch with only a handful of commits on top of main).
pub(super) fn is_patch_equivalent_in_window_in(
    wt: &Path,
    sha: &str,
    branch: &str,
    window: usize,
) -> Result<bool, String> {
    if window == 0 {
        return Ok(false);
    }

    let target_id = match git_patch_id(wt, sha)? {
        Some(id) => id,
        None => return Ok(false),
    };

    let max_count = format!("--max-count={window}");
    let output = crate::git_exec::run_git(wt, &["rev-list", &max_count, branch])?;

    if !output.status.success() {
        return Ok(false);
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let commit_sha = line.trim();
        if commit_sha.is_empty() {
            continue;
        }
        if let Ok(Some(candidate_id)) = git_patch_id(wt, commit_sha) {
            if candidate_id == target_id {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Check whether the tree changes introduced by `sha` are already present
/// on `branch` in `wt`.
///
/// This compares the blob contents at every path changed by `sha`. A commit
/// message trailer or a path-only diff is not sufficient proof that the branch
/// contains the reviewed content.
pub(super) fn tree_matches_after(wt: &Path, sha: &str, branch: &str) -> Result<bool, String> {
    // Fast path: if sha is already an ancestor, its changes are present.
    if git_commit_is_ancestor_in(wt, sha, branch).unwrap_or(false) {
        return Ok(true);
    }

    // Compute the diff from sha's parent → sha (the changes we expect).
    let parent_commit = match git_stdout_in(wt, &["rev-parse", &format!("{sha}^")]) {
        Ok(commit) => commit,
        Err(_) => {
            // Root commit or unreachable; fall back to ancestry check only.
            return Ok(false);
        }
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
        return Ok(false);
    }

    let expected_files: Vec<String> = String::from_utf8_lossy(&diff_expected.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    if expected_files.is_empty() {
        // Empty commit; by definition it matches.
        return Ok(true);
    }

    let mut exact_match = true;
    let mut content_present = true;
    for file in &expected_files {
        let expected = blob_bytes_at(wt, sha, &file)?;
        let actual = blob_bytes_at(wt, branch, &file)?;
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
        return Ok(true);
    }

    let branch_head = git_stdout_in(wt, &["rev-parse", branch])?;
    let worktree_head = git_stdout_in(wt, &["rev-parse", "HEAD"])?;
    if branch_head == worktree_head && reviewed_patch_reverses_cleanly(wt, &parent_commit, sha)? {
        return Ok(true);
    }

    Ok(content_present)
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
