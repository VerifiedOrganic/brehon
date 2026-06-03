//! Git synchronization: merge-target types, branch syncing, worktree alignment.

use serde_json::Value;
use std::path::{Path, PathBuf};

use super::paths::{project_root, read_task, resolve_project_path};
use super::worktree_ops::candidate_worker_worktree_paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MergeTargetSyncStatus {
    AlreadyCurrent,
    Reset,
}

#[derive(Debug, Clone)]
pub(super) struct AssignmentSeedSyncResult {
    pub worktree_path: PathBuf,
    pub worker_branch: String,
    pub status: MergeTargetSyncStatus,
    pub target_ref: String,
    pub target_kind: AssignmentSeedKind,
    pub head_before: String,
    pub head_after: String,
    pub preserved_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AssignmentSeedKind {
    MergeTarget,
    LatestCommit,
}

impl AssignmentSeedKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::MergeTarget => "merge_target",
            Self::LatestCommit => "latest_commit",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::MergeTarget => "merge_target",
            Self::LatestCommit => "task latest_commit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MergeTargetBaseSyncStatus {
    AlreadyCurrent,
    Merged,
}

#[derive(Debug, Clone)]
pub(super) struct MergeTargetBaseSyncResult {
    pub integration_worktree: PathBuf,
    pub integration_branch: String,
    pub status: MergeTargetBaseSyncStatus,
    pub base_branch: String,
    pub head_before: String,
    pub head_after: String,
}

pub(super) fn run_git_in(worktree_path: &Path, args: &[&str]) -> Result<String, String> {
    let output = crate::git_exec::run_git(worktree_path, args)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!(
            "git {} failed in '{}': {}",
            args.join(" "),
            worktree_path.display(),
            detail
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn detect_default_branch_in(cwd: &Path) -> Result<String, String> {
    if let Ok(branch) = run_git_in(cwd, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(stripped) = branch.strip_prefix("refs/remotes/origin/") {
            return Ok(stripped.to_string());
        }
    }

    for candidate in ["main", "master", "develop"] {
        let ref_path = format!("refs/heads/{candidate}");
        if run_git_in(cwd, &["rev-parse", "--verify", &ref_path]).is_ok() {
            return Ok(candidate.to_string());
        }
    }

    Ok("main".to_string())
}

pub(super) fn merge_target_base_branch(
    task: &serde_json::Map<String, Value>,
) -> Result<Option<String>, String> {
    let root = project_root().ok_or_else(|| {
        "Cannot resolve merge-target base branch: project root is unavailable.".to_string()
    })?;
    let default_branch = detect_default_branch_in(&root)?;

    let Some(parent_id) = task.get("parent_id").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let Some(parent) = read_task(parent_id) else {
        return Ok(Some(default_branch));
    };
    let parent_type = parent
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    if parent_type != "epic" {
        return Ok(Some(default_branch));
    }

    if let Some(initiative_id) = parent.get("parent_id").and_then(|v| v.as_str()) {
        if let Some(initiative) = read_task(initiative_id) {
            if initiative
                .get("task_type")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == "initiative")
            {
                if let Some(branch) = initiative
                    .get("integration_branch")
                    .and_then(|v| v.as_str())
                    .filter(|value| !value.is_empty())
                {
                    return Ok(Some(branch.to_string()));
                }
                return Err(format!(
                    "Initiative {initiative_id} is missing integration_branch. \
                     Reconcile the initiative hierarchy before assigning child epic work so merge targets do not fall back to the default branch."
                ));
            }
        }
    }

    Ok(Some(default_branch))
}

pub(super) fn sync_merge_target_branch_to_parent_base(
    task: &serde_json::Map<String, Value>,
    merge_target: &str,
) -> Result<Option<MergeTargetBaseSyncResult>, String> {
    let _ = project_root().ok_or_else(|| {
        "Cannot sync merge-target branch: project root is unavailable.".to_string()
    })?;
    let Some(base_branch) = merge_target_base_branch(task)? else {
        return Ok(None);
    };
    if merge_target == base_branch {
        return Ok(None);
    }

    let Some(parent_id) = task.get("parent_id").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let Some(parent) = read_task(parent_id) else {
        return Ok(None);
    };
    let Some(integration_worktree) = parent
        .get("integration_worktree")
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .and_then(|path| resolve_project_path(&path))
    else {
        return Ok(None);
    };

    let repo = git2::Repository::open(&integration_worktree).map_err(|e| {
        format!(
            "Cannot open integration worktree '{}' for merge_target '{}': {}",
            integration_worktree.display(),
            merge_target,
            e
        )
    })?;

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts)).map_err(|e| {
        format!(
            "Cannot inspect integration worktree '{}' for merge_target '{}': {}",
            integration_worktree.display(),
            merge_target,
            e
        )
    })?;
    if !statuses.is_empty() {
        return Err(format!(
            "Cannot assign task: merge_target '{}' integration worktree '{}' is dirty. Clean or archive it first.",
            merge_target,
            integration_worktree.display()
        ));
    }

    if repo.state() != git2::RepositoryState::Clean {
        return Err(format!(
            "Cannot assign task: merge_target '{}' integration worktree '{}' is in mid-operation state ({:?}).",
            merge_target,
            integration_worktree.display(),
            repo.state()
        ));
    }

    let current_branch = repo
        .head()
        .map_err(|e| {
            format!(
                "Cannot read HEAD for integration worktree '{}' (merge_target '{}'): {}",
                integration_worktree.display(),
                merge_target,
                e
            )
        })?
        .shorthand()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            format!(
                "Cannot assign task: merge_target '{}' integration worktree '{}' is detached.",
                merge_target,
                integration_worktree.display()
            )
        })?
        .to_string();
    if current_branch != merge_target {
        return Err(format!(
            "Cannot assign task: merge_target '{}' integration worktree '{}' is on '{}' instead of '{}'.",
            merge_target,
            integration_worktree.display(),
            current_branch,
            merge_target
        ));
    }

    let head_before = run_git_in(&integration_worktree, &["rev-parse", "HEAD"])?;
    let base_branch_oid = repo
        .refname_to_id(&format!("refs/heads/{base_branch}"))
        .map_err(|e| {
            format!(
                "Cannot assign task: base branch '{}' is not available while syncing merge_target '{}': {}",
                base_branch, merge_target, e
            )
        })?;
    let merge_target_oid = repo
        .head()
        .map_err(|e| {
            format!(
                "Cannot read HEAD for merge_target '{}' in '{}': {}",
                merge_target,
                integration_worktree.display(),
                e
            )
        })?
        .target()
        .ok_or_else(|| {
            format!(
                "Cannot read current HEAD oid for merge_target '{}' in '{}'.",
                merge_target,
                integration_worktree.display()
            )
        })?;

    let already_current = repo
        .graph_descendant_of(merge_target_oid, base_branch_oid)
        .map_err(|e| {
            format!(
                "Cannot compare merge_target '{}' against base branch '{}' in '{}': {}",
                merge_target,
                base_branch,
                integration_worktree.display(),
                e
            )
        })?;

    if already_current {
        return Ok(Some(MergeTargetBaseSyncResult {
            integration_worktree,
            integration_branch: merge_target.to_string(),
            status: MergeTargetBaseSyncStatus::AlreadyCurrent,
            base_branch,
            head_before: head_before.clone(),
            head_after: head_before,
        }));
    }

    if let Err(err) = run_git_in(&integration_worktree, &["merge", "--no-edit", &base_branch]) {
        let _ = run_git_in(&integration_worktree, &["merge", "--abort"]);
        return Err(format!(
            "Cannot assign task: failed to sync merge_target '{}' with base branch '{}': {}",
            merge_target, base_branch, err
        ));
    }

    let head_after = run_git_in(&integration_worktree, &["rev-parse", "HEAD"])?;
    Ok(Some(MergeTargetBaseSyncResult {
        integration_worktree,
        integration_branch: merge_target.to_string(),
        status: MergeTargetBaseSyncStatus::Merged,
        base_branch,
        head_before,
        head_after,
    }))
}

pub(super) fn sync_worker_worktree_to_assignment_seed(
    worker_name: &str,
    target_ref: &str,
    target_kind: AssignmentSeedKind,
) -> Result<AssignmentSeedSyncResult, String> {
    let matches = candidate_worker_worktree_paths(worker_name);
    if matches.is_empty() {
        return Err(format!(
            "Cannot assign task to worker '{}': no worktree found for assignment seed '{}'.",
            worker_name, target_ref
        ));
    }
    if matches.len() > 1 {
        let match_list: Vec<_> = matches.iter().map(|p| p.display().to_string()).collect();
        return Err(format!(
            "Cannot assign task to worker '{}': ambiguous worktree match while syncing assignment seed '{}': {}",
            worker_name,
            target_ref,
            match_list.join(", ")
        ));
    }

    let worktree_path = matches.into_iter().next().unwrap();
    let repo = git2::Repository::open(&worktree_path).map_err(|e| {
        format!(
            "Cannot open worker worktree '{}' for '{}': {}",
            worktree_path.display(),
            worker_name,
            e
        )
    })?;

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts)).map_err(|e| {
        format!(
            "Cannot inspect worker worktree '{}' for '{}': {}",
            worktree_path.display(),
            worker_name,
            e
        )
    })?;
    if !statuses.is_empty() {
        return Err(format!(
            "Cannot assign task to worker '{}': worktree '{}' is dirty before syncing {} '{}'. Clean or archive it first.",
            worker_name,
            worktree_path.display(),
            target_kind.display_name(),
            target_ref
        ));
    }

    if repo.state() != git2::RepositoryState::Clean {
        return Err(format!(
            "Cannot assign task to worker '{}': worktree '{}' is in mid-operation state ({:?}) before syncing {} '{}'.",
            worker_name,
            worktree_path.display(),
            repo.state(),
            target_kind.display_name(),
            target_ref
        ));
    }

    let head = repo
        .head()
        .map_err(|e| format!("Cannot read HEAD for '{}': {}", worktree_path.display(), e))?;
    let worker_branch = head
        .shorthand()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            format!(
                "Cannot assign task to worker '{}': worktree '{}' is detached before syncing {} '{}'.",
                worker_name,
                worktree_path.display(),
                target_kind.display_name(),
                target_ref
            )
        })?
        .to_string();

    let head_before = run_git_in(&worktree_path, &["rev-parse", "HEAD"])?;
    let target_oid = match target_kind {
        AssignmentSeedKind::MergeTarget => repo
            .refname_to_id(&format!("refs/heads/{target_ref}"))
            .map_err(|e| {
                format!(
                    "Cannot assign task to worker '{}': merge_target '{}' is not a local branch: {}",
                    worker_name, target_ref, e
                )
            })?,
        AssignmentSeedKind::LatestCommit => repo
            .revparse_single(target_ref)
            .map_err(|e| {
                format!(
                    "Cannot assign task to worker '{}': latest_commit '{}' is not available in repo history: {}",
                    worker_name, target_ref, e
                )
            })?
            .id(),
    };
    let head_oid = repo
        .head()
        .map_err(|e| format!("Cannot read HEAD for '{}': {}", worktree_path.display(), e))?
        .target()
        .ok_or_else(|| {
            format!(
                "Cannot read current HEAD oid for worker '{}' in '{}'.",
                worker_name,
                worktree_path.display()
            )
        })?;

    if head_oid == target_oid {
        return Ok(AssignmentSeedSyncResult {
            worktree_path,
            worker_branch,
            status: MergeTargetSyncStatus::AlreadyCurrent,
            target_ref: target_ref.to_string(),
            target_kind,
            head_before: head_before.clone(),
            head_after: head_before,
            preserved_ref: None,
        });
    }

    let head_contains_target = repo
        .graph_descendant_of(head_oid, target_oid)
        .map_err(|e| {
            format!(
                "Cannot compare worker branch '{}' to {} '{}' for '{}': {}",
                worker_branch,
                target_kind.display_name(),
                target_ref,
                worker_name,
                e
            )
        })?;

    let target_contains_head = repo
        .graph_descendant_of(target_oid, head_oid)
        .map_err(|e| {
            format!(
                "Cannot compare {} '{}' to worker branch '{}' for '{}': {}",
                target_kind.display_name(),
                target_ref,
                worker_branch,
                worker_name,
                e
            )
        })?;

    let mut preserved_ref = None;
    if head_contains_target && !target_contains_head {
        let archive_ref = format!("refs/brehon/archive/{worker_name}/{head_before}");
        repo.reference(
            &archive_ref,
            head_oid,
            true,
            "preserve worker head before resetting to merge target",
        )
        .map_err(|e| {
            format!(
                "Cannot preserve worker branch '{}' for '{}': failed to create archive ref '{}': {}",
                worker_branch, worker_name, archive_ref, e
            )
        })?;
        preserved_ref = Some(archive_ref);
    } else if !target_contains_head {
        let archive_ref = format!("refs/brehon/archive/{worker_name}/{head_before}");
        repo.reference(
            &archive_ref,
            head_oid,
            true,
            "preserve diverged worker head before resetting to merge target",
        )
        .map_err(|e| {
            format!(
                "Cannot preserve diverged worker branch '{}' for '{}': failed to create archive ref '{}': {}",
                worker_branch, worker_name, archive_ref, e
            )
        })?;
        preserved_ref = Some(archive_ref);
    }

    if let Err(err) = run_git_in(&worktree_path, &["reset", "--hard", target_ref]) {
        return Err(format!(
            "Cannot assign task to worker '{}': failed to reset worker branch '{}' to {} '{}': {}",
            worker_name,
            worker_branch,
            target_kind.display_name(),
            target_ref,
            err
        ));
    }

    let head_after = run_git_in(&worktree_path, &["rev-parse", "HEAD"])?;
    Ok(AssignmentSeedSyncResult {
        worktree_path,
        worker_branch,
        status: MergeTargetSyncStatus::Reset,
        target_ref: target_ref.to_string(),
        target_kind,
        head_before,
        head_after,
        preserved_ref,
    })
}
