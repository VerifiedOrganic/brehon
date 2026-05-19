use super::helpers::{
    brehon_root, create_preflight_lease, git_output_in, remove_git_worktree_force,
    remove_preflight_lease, sweep_stale_preflight_worktrees, workspace_root,
};
use crate::tools::git_cherry_pick::can_skip_failed_cherry_pick_as_empty;

#[derive(Debug, Clone, Default)]
pub(super) struct ResolvedReviewCommitSet {
    pub(super) base_commit: String,
    pub(super) merge_target_head: String,
    pub(super) commits: Vec<String>,
    /// True when commit enumeration for merge_target..reviewed_commit succeeded.
    /// When this is true and `commits` is empty, downstream consumers should
    /// treat the reviewed set as intentionally empty rather than falling back to
    /// replaying `reviewed_tip` as a single commit.
    pub(super) commit_set_resolved: bool,
}

pub(super) fn resolve_review_commit_set(
    task_id: &str,
    merge_target: &str,
    reviewed_commit: &str,
) -> Result<ResolvedReviewCommitSet, String> {
    let Some(repo_root) = workspace_root() else {
        return Err(format!(
            "Cannot resolve reviewed commit set for task {task_id}: no git workspace is available."
        ));
    };

    let merge_target_head =
        git_output_in(&repo_root, &["rev-parse", merge_target]).map_err(|err| {
            format!(
                "Cannot resolve merge target head for task {task_id} on '{merge_target}': {err}"
            )
        })?;
    let base_commit =
        git_output_in(&repo_root, &["merge-base", merge_target, reviewed_commit]).map_err(
            |err| {
                format!(
                    "Cannot compute merge base for task {task_id} between '{merge_target}' and reviewed commit {reviewed_commit}: {err}"
                )
            },
        )?;
    let repo = git2::Repository::discover(&repo_root).map_err(|err| {
        format!(
            "Cannot inspect reviewed commit set for task {task_id} in '{}': {err}",
            repo_root.display()
        )
    })?;
    let commits_result = git_output_in(
        &repo_root,
        &[
            "rev-list",
            "--reverse",
            &format!("{merge_target}..{reviewed_commit}"),
        ],
    )
    .and_then(|stdout| -> Result<Vec<String>, String> {
        let mut commits = Vec::new();
        for commit in stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if commit_is_empty(&repo, commit)? {
                continue;
            }
            commits.push(commit.to_string());
        }
        Ok(filter_commits_that_become_empty_when_replayed(
            task_id,
            &repo_root,
            merge_target,
            &commits,
        ))
    });
    if let Err(err) = commits_result.as_ref() {
        tracing::warn!(
            task_id = %task_id,
            reviewed_commit = %reviewed_commit,
            merge_target = %merge_target,
            error = %err,
            "Failed to resolve reviewed commit chain; falling back to an empty reviewed commit set"
        );
    }
    let commit_set_resolved = commits_result.is_ok();
    let commits = commits_result.unwrap_or_default();

    Ok(ResolvedReviewCommitSet {
        base_commit,
        merge_target_head,
        commits,
        commit_set_resolved,
    })
}

fn commit_is_empty(repo: &git2::Repository, commit: &str) -> Result<bool, String> {
    let oid = git2::Oid::from_str(commit)
        .map_err(|err| format!("Cannot parse reviewed commit '{commit}': {err}"))?;
    let git_commit = repo
        .find_commit(oid)
        .map_err(|err| format!("Cannot load reviewed commit '{commit}': {err}"))?;
    if git_commit.parent_count() == 0 {
        return Ok(false);
    }
    // Intentional first-parent comparison: resubmission/checkpoint markers in this
    // workflow are linear commits, and matching the first-parent tree is enough to
    // identify a no-op commit that should not be replayed during review preflight.
    let tree = git_commit.tree().map_err(|err| {
        format!(
            "Cannot read tree for reviewed commit '{}': {err}",
            git_commit.id()
        )
    })?;
    let parent_tree = git_commit
        .parent(0)
        .and_then(|parent| parent.tree())
        .map_err(|err| {
            format!(
                "Cannot read first-parent tree for reviewed commit '{}': {err}",
                git_commit.id()
            )
        })?;
    Ok(tree.id() == parent_tree.id())
}

fn filter_commits_that_become_empty_when_replayed(
    task_id: &str,
    repo_root: &std::path::Path,
    merge_target: &str,
    commits: &[String],
) -> Vec<String> {
    if commits.is_empty() {
        return Vec::new();
    }

    let Some(brehon_root) = brehon_root() else {
        tracing::warn!(
            task_id = %task_id,
            merge_target = %merge_target,
            "No BREHON_ROOT available while filtering replay-empty reviewed commits; keeping the original ordered reviewed set"
        );
        return commits.to_vec();
    };
    let preflight_base = brehon_root.join("runtime").join("preflight");
    if let Err(err) = std::fs::create_dir_all(&preflight_base) {
        tracing::warn!(
            task_id = %task_id,
            merge_target = %merge_target,
            path = %preflight_base.display(),
            error = %err,
            "Failed to create preflight dir while filtering replay-empty reviewed commits; keeping the original ordered reviewed set"
        );
        return commits.to_vec();
    }

    sweep_stale_preflight_worktrees(&repo_root, &preflight_base, task_id);
    let temp_worktree = preflight_base.join(format!(
        "{}-resolve-{}",
        task_id,
        uuid::Uuid::new_v4().simple()
    ));
    let temp_entry_name = temp_worktree
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if let Err(err) = create_preflight_lease(&preflight_base, temp_entry_name) {
        tracing::warn!(
            task_id = %task_id,
            error = %err,
            "Failed to create preflight lease; cannot safely proceed with replay filter"
        );
        return commits.to_vec();
    }
    let Some(temp_worktree_str) = temp_worktree.to_str() else {
        tracing::warn!(
            task_id = %task_id,
            merge_target = %merge_target,
            path = %temp_worktree.display(),
            "Replay filter worktree path is not valid UTF-8; keeping the original ordered reviewed set"
        );
        remove_preflight_lease(&preflight_base, temp_entry_name);
        return commits.to_vec();
    };
    let setup = git_output_in(
        repo_root,
        &[
            "worktree",
            "add",
            "--detach",
            temp_worktree_str,
            merge_target,
        ],
    );
    if let Err(err) = setup {
        tracing::warn!(
            task_id = %task_id,
            merge_target = %merge_target,
            error = %err,
            "Failed to create replay filter worktree; keeping the original ordered reviewed set"
        );
        remove_preflight_lease(&preflight_base, temp_entry_name);
        return commits.to_vec();
    }

    let filtered = (|| -> Result<Vec<String>, String> {
        let mut filtered = Vec::new();
        for commit in commits {
            let cherry_pick = git_output_in(&temp_worktree, &["cherry-pick", commit]);
            match cherry_pick {
                Ok(_) => filtered.push(commit.clone()),
                Err(err) if can_skip_failed_cherry_pick_as_empty(&temp_worktree, &err) => {
                    git_output_in(&temp_worktree, &["cherry-pick", "--skip"]).map_err(|skip_err| {
                        format!(
                            "Failed to skip replay-empty reviewed commit '{commit}' while resolving the reviewed set for task {task_id}: {skip_err}"
                        )
                    })?;
                }
                Err(err) => {
                    let _ = git_output_in(&temp_worktree, &["cherry-pick", "--abort"]);
                    return Err(format!(
                        "Failed to replay reviewed commit '{commit}' while resolving the reviewed set for task {task_id}: {err}"
                    ));
                }
            }
        }
        Ok(filtered)
    })();
    remove_git_worktree_force(repo_root, &temp_worktree);
    remove_preflight_lease(&preflight_base, temp_entry_name);

    match filtered {
        Ok(filtered) => filtered,
        Err(err) => {
            tracing::warn!(
                task_id = %task_id,
                merge_target = %merge_target,
                error = %err,
                "Failed to filter replay-empty reviewed commits; keeping the original ordered reviewed set"
            );
            commits.to_vec()
        }
    }
}

pub(super) fn preview_commit_integration_conflicts(
    task_id: &str,
    merge_target: &str,
    reviewed_commits: &[String],
    reviewed_commit_set_resolved: bool,
    reviewed_tip: &str,
) -> Result<Vec<String>, String> {
    let Some(repo_root) = workspace_root() else {
        return Ok(Vec::new());
    };
    let Ok(repo_probe) = git2::Repository::discover(&repo_root) else {
        return Ok(Vec::new());
    };
    if repo_probe
        .find_reference(&format!("refs/heads/{merge_target}"))
        .is_err()
    {
        return Ok(Vec::new());
    }
    let Ok(reviewed_oid) = git2::Oid::from_str(reviewed_tip) else {
        return Ok(Vec::new());
    };
    if repo_probe.find_commit(reviewed_oid).is_err() {
        return Ok(Vec::new());
    }
    let brehon_root = brehon_root().ok_or_else(|| {
        "No BREHON_ROOT available. Cannot allocate integration preflight workspace.".to_string()
    })?;
    let preflight_base = brehon_root.join("runtime").join("preflight");
    std::fs::create_dir_all(&preflight_base).map_err(|err| {
        format!(
            "Failed to create integration preflight dir '{}': {err}",
            preflight_base.display()
        )
    })?;

    sweep_stale_preflight_worktrees(&repo_root, &preflight_base, task_id);
    let temp_worktree =
        preflight_base.join(format!("{}-{}", task_id, uuid::Uuid::new_v4().simple()));
    let temp_entry_name = temp_worktree
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if let Err(err) = create_preflight_lease(&preflight_base, temp_entry_name) {
        return Err(format!(
            "Failed to create integration preflight lease for task {task_id}: {err}"
        ));
    }

    let setup = git_output_in(
        &repo_root,
        &[
            "worktree",
            "add",
            "--detach",
            temp_worktree
                .to_str()
                .ok_or_else(|| "preflight path is not valid UTF-8".to_string())?,
            merge_target,
        ],
    );
    if let Err(err) = setup {
        remove_preflight_lease(&preflight_base, temp_entry_name);
        return Err(format!(
            "Cannot preview integration of task {task_id} into '{merge_target}': {err}"
        ));
    }

    let result = (|| -> Result<Vec<String>, String> {
        let temp_repo = git2::Repository::open(&temp_worktree).map_err(|err| {
            format!(
                "Cannot open integration preflight worktree '{}': {err}",
                temp_worktree.display()
            )
        })?;
        let commits_to_apply = if reviewed_commits.is_empty() {
            if reviewed_commit_set_resolved {
                return Ok(Vec::new());
            }
            let head_oid = temp_repo
                .head()
                .map_err(|err| {
                    format!(
                        "Cannot read HEAD in integration preflight worktree '{}': {err}",
                        temp_worktree.display()
                    )
                })?
                .target()
                .ok_or_else(|| {
                    format!(
                        "Cannot read HEAD oid in integration preflight worktree '{}'.",
                        temp_worktree.display()
                    )
                })?;
            let already_integrated = if head_oid == reviewed_oid {
                true
            } else {
                temp_repo
                    .graph_descendant_of(head_oid, reviewed_oid)
                    .map_err(|err| {
                        format!(
                            "Cannot compare reviewed commit '{}' against merge_target '{}': {err}",
                            reviewed_tip, merge_target
                        )
                    })?
            };
            if already_integrated {
                return Ok(Vec::new());
            }
            vec![reviewed_tip.to_string()]
        } else {
            reviewed_commits.to_vec()
        };

        for commit in &commits_to_apply {
            let commit_oid = git2::Oid::from_str(commit).map_err(|err| {
                format!("Cannot parse reviewed commit '{commit}' for task {task_id}: {err}")
            })?;
            let head_oid = temp_repo
                .head()
                .map_err(|err| {
                    format!(
                        "Cannot read HEAD in integration preflight worktree '{}': {err}",
                        temp_worktree.display()
                    )
                })?
                .target()
                .ok_or_else(|| {
                    format!(
                        "Cannot read HEAD oid in integration preflight worktree '{}'.",
                        temp_worktree.display()
                    )
                })?;

            let already_integrated = if head_oid == commit_oid {
                true
            } else {
                temp_repo
                    .graph_descendant_of(head_oid, commit_oid)
                    .map_err(|err| {
                        format!(
                            "Cannot compare reviewed commit '{}' against merge_target '{}': {err}",
                            commit, merge_target
                        )
                    })?
            };
            if already_integrated {
                continue;
            }

            let cherry_pick = git_output_in(&temp_worktree, &["cherry-pick", commit]);
            if cherry_pick.is_ok() {
                continue;
            }
            if cherry_pick
                .as_ref()
                .err()
                .is_some_and(|err| can_skip_failed_cherry_pick_as_empty(&temp_worktree, err))
            {
                git_output_in(&temp_worktree, &["cherry-pick", "--skip"]).map_err(|err| {
                    format!(
                        "Failed to skip already-applied reviewed commit '{}' during integration preflight for task {}: {}",
                        commit, task_id, err
                    )
                })?;
                continue;
            }

            let cherry_err = cherry_pick.err().unwrap_or_else(|| {
                format!(
                    "Integration preflight failed for reviewed commit '{}' into '{}'",
                    commit, merge_target
                )
            });
            let conflicts =
                git_output_in(&temp_worktree, &["diff", "--name-only", "--diff-filter=U"])
                    .ok()
                    .map(|stdout| {
                        stdout
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(String::from)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
            let _ = git_output_in(&temp_worktree, &["cherry-pick", "--abort"]);
            return if conflicts.is_empty() {
                Err(format!(
                    "Integration preflight failed for reviewed commit '{}' into '{}': {}",
                    commit, merge_target, cherry_err
                ))
            } else {
                Ok(conflicts)
            };
        }
        Ok(Vec::new())
    })();

    remove_git_worktree_force(&repo_root, &temp_worktree);
    remove_preflight_lease(&preflight_base, temp_entry_name);
    result
}
