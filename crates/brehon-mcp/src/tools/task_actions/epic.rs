//! Epic/initiative integration: worktrees, branch management, conflict tracking, merge verification.

use std::path::{Path, PathBuf};

use serde_json::Value;

use brehon_ports::GitOperations;
use brehon_types::is_terminal_task_status;

use super::git_ops::{
    detect_default_branch, detect_default_branch_in, detect_remote_merge_status,
    ensure_brehon_ignored_in_repo, git_branch_exists_in, git_commit_is_ancestor,
    git_commit_is_ancestor_in, git_run_ok_in, git_status_porcelain_in, git_stdout, git_stdout_in,
    non_brehon_status_entries, unmerged_files, MergeStatus,
};
use super::lifecycle::{is_epic, is_initiative, reconcile_dependency_states_with_task_lock};
use super::locking::acquire_task_lock;
use super::paths::{
    brehon_root_dir, brehon_worktrees_root, ensure_brehon_worktree_path, project_root,
    resolve_project_path,
};
use super::persistence::{read_all_tasks, read_task, write_task};
use crate::tools::verification::{
    read_review_state, read_round_request, reviewed_commits, ReviewRequestFile,
};

/// Prefix used for ANY integration conflict blocker message — supervisor- or
/// worker-owned. Code that checks whether `blockers` carries a recorded
/// integration conflict should use `is_integration_conflict_blocker` (a
/// case-insensitive starts_with match against this prefix) so future label
/// tweaks don't silently drop matches. The blocker is now phrased neutrally;
/// the conflict's `owner` field is the source of truth for routing.
pub(super) const INTEGRATION_CONFLICT_BLOCKER_PREFIX: &str = "Integration conflict";

/// Map a conflict `source` to the agent that owns the resolution by default.
///
/// * `review_preflight` — the worker can almost always self-resolve by
///   rebasing onto the merge target and re-requesting review. Marking it
///   supervisor-owned floods the supervisor's queue with conflicts they
///   cannot meaningfully act on, and (before the assignee-preserving fix)
///   stranded the worker. Default to `worker`.
/// * `approved_integration` — the conflict happened during cherry-pick into
///   the epic worktree, which only the supervisor has write access to.
///   Default to `supervisor`.
/// * `worker_unmerged` — the worker's branch is in a state Brehon can't
///   reason about (mid-rebase, detached, etc.); a supervisor must inspect.
///   Default to `supervisor`.
/// * Anything unrecognised — be conservative and default to `supervisor`
///   so unknown failure modes still get visible attention.
pub(super) fn default_conflict_owner(source: &str) -> &'static str {
    match source {
        "review_preflight" => "worker",
        _ => "supervisor",
    }
}

pub(super) fn task_review_dir(task_id: &str) -> Option<PathBuf> {
    brehon_root_dir().map(|root| root.join("runtime").join("reviews").join(task_id))
}

pub(super) fn latest_review_round_with_request(task_id: &str) -> Option<u32> {
    let dir = task_review_dir(task_id)?;
    let mut rounds = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let round = name.strip_prefix("round-")?.parse::<u32>().ok()?;
            entry.path().join("request.json").is_file().then_some(round)
        })
        .collect::<Vec<_>>();
    rounds.sort_unstable();
    rounds.pop()
}

pub(super) fn read_current_review_request(task_id: &str) -> Option<ReviewRequestFile> {
    if let Some(state) = read_review_state(task_id) {
        if let Some(request) = read_round_request(task_id, state.current_round) {
            return Some(request);
        }
    }
    latest_review_round_with_request(task_id).and_then(|round| read_round_request(task_id, round))
}

pub(super) fn verify_merge_ready(
    task_id: &str,
    task_data: Option<&serde_json::Map<String, Value>>,
) -> Result<(String, String, MergeStatus), String> {
    let request = read_current_review_request(task_id).ok_or_else(|| {
        format!(
            "Cannot mark task {task_id} as merged without review metadata. \
             Start or re-run review with commit provenance first."
        )
    })?;
    let reviewed_commit = request.commit.trim().to_string();
    let reviewed_commit_set = reviewed_commits(&request);
    let resolved_empty_commit_set =
        request.resolved_empty_commit_set && reviewed_commit_set.is_empty();
    if reviewed_commit.is_empty() || (reviewed_commit_set.is_empty() && !resolved_empty_commit_set)
    {
        return Err(format!(
            "Cannot mark task {task_id} as merged because the approved review recorded no commit. \
             Re-run review with commit=<hash>."
        ));
    }

    // Determine merge target: use merge_target from task, or fall back to default branch
    let merge_target = if let Some(data) = task_data {
        if let Some(target) = data.get("merge_target").and_then(|v| v.as_str()) {
            if !target.is_empty() {
                target.to_string()
            } else {
                detect_default_branch()?
            }
        } else {
            detect_default_branch()?
        }
    } else {
        detect_default_branch()?
    };

    let branch = git_stdout(&["branch", "--show-current"])?;
    if branch != merge_target {
        return Err(format!(
            "Cannot mark task {task_id} as merged while current branch is '{branch}'. \
             Integrate reviewed commit {reviewed_commit} into {merge_target} first."
        ));
    }

    if !resolved_empty_commit_set {
        for commit in &reviewed_commit_set {
            let on_target = git_commit_is_ancestor(commit, "HEAD")?;
            if !on_target {
                return Err(format!(
                    "Cannot mark task {task_id} as merged: reviewed commit {commit} is not reachable from HEAD on {merge_target}. \
                     Merge or cherry-pick the full reviewed task delta into {merge_target} first."
                ));
            }
        }
    }

    // For epic integration branches, we still report remote status but verify branch ancestry
    let merged_commit = if resolved_empty_commit_set {
        git_stdout(&["rev-parse", "HEAD"])?
    } else {
        // reviewed_commit_set is expected to contain only non-empty substantive commits.
        // Empty marker commits should appear only as request.commit (the review tip).
        reviewed_commit_set
            .last()
            .cloned()
            .unwrap_or(reviewed_commit)
    };
    let merge_status = if resolved_empty_commit_set {
        MergeStatus::RemoteStatusUnknown
    } else {
        detect_remote_merge_status(&merged_commit, &merge_target)
    };
    Ok((merge_target, merged_commit, merge_status))
}

pub(super) fn integration_conflict_blockers(
    merge_target: &str,
    reviewed_commit: &str,
    conflicting_files: &[String],
    source: &str,
) -> String {
    let files = if conflicting_files.is_empty() {
        "unknown files".to_string()
    } else {
        conflicting_files.join(", ")
    };
    let next_step = match source {
        "review_preflight" => {
            "Worker must rebase the task branch onto the merge target, resolve the listed conflicts locally, and re-request review. The conflict marker auto-clears on the next clean preflight; escalate to supervisor only if rebase repeatedly fails."
        }
        "approved_integration" => {
            "Supervisor must resolve the integration conflict in the epic worktree, then re-run task action=integrate."
        }
        "worker_unmerged" => {
            "Supervisor must inspect the conflicting worktree state and decide how to continue before the task can be reassigned."
        }
        _ => "Supervisor must resolve this integration conflict before the task can continue.",
    };
    format!(
        "{INTEGRATION_CONFLICT_BLOCKER_PREFIX} for reviewed commit {reviewed_commit} against '{merge_target}'. Conflicting files: {files}. {next_step}"
    )
}

pub(crate) fn task_has_supervisor_integration_conflict(
    task: &serde_json::Map<String, Value>,
) -> bool {
    task.get("integration_conflict")
        .and_then(|value| value.get("owner"))
        .and_then(|value| value.as_str())
        == Some("supervisor")
}

fn is_integration_conflict_blocker(blockers: &str) -> bool {
    blockers
        .trim_start()
        .to_ascii_lowercase()
        .starts_with(&INTEGRATION_CONFLICT_BLOCKER_PREFIX.to_ascii_lowercase())
}

pub(crate) fn task_has_integration_conflict_recovery_marker(
    task: &serde_json::Map<String, Value>,
) -> bool {
    task_has_supervisor_integration_conflict(task)
        || task
            .get("blockers")
            .and_then(|value| value.as_str())
            .is_some_and(is_integration_conflict_blocker)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_supervisor_integration_conflict(
    task: &mut serde_json::Map<String, Value>,
    desired_status: &str,
    merge_target: &str,
    reviewed_commit: &str,
    reviewed_commits: &[String],
    conflicting_files: &[String],
    source: &str,
    worker_hint: Option<&str>,
) {
    // Conflict ownership is tracked solely via the `integration_conflict.owner`
    // field; assignee/review_owner are NOT cleared. Earlier versions nulled
    // both fields, which broke two production cases:
    //   1. The dashboard rendered in_progress tasks with no assignee, hiding
    //      that a worker was actively rebasing/fixing the conflict.
    //   2. When the worker self-resolved the conflict and called `task
    //      complete`, the assignee-match guard rejected them because their
    //      identity had been wiped, deadlocking the run until the supervisor
    //      manually intervened.
    // `previous_worker` is still recorded inside the conflict blob for the
    // TUI's "previously assigned" affordance and as a defensive fallback for
    // legacy task JSON that already has a null assignee.
    let previous_worker = task
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            task.get("review_owner")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            worker_hint
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });

    task.insert("status".into(), Value::String(desired_status.to_string()));
    task.insert(
        "activity".into(),
        Value::String("integration_conflict".to_string()),
    );
    task.insert(
        "blockers".into(),
        Value::String(integration_conflict_blockers(
            merge_target,
            reviewed_commit,
            conflicting_files,
            source,
        )),
    );
    task.insert(
        "integration_conflict".into(),
        serde_json::json!({
            "owner": default_conflict_owner(source),
            "source": source,
            "merge_target": merge_target,
            "reviewed_commit": reviewed_commit,
            "reviewed_commits": reviewed_commits,
            "conflicting_files": conflicting_files,
            "previous_worker": previous_worker,
            "recorded_at": chrono::Utc::now().to_rfc3339(),
        }),
    );
    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn mark_task_supervisor_integration_conflict(
    task_id: &str,
    desired_status: &str,
    merge_target: &str,
    reviewed_commit: &str,
    reviewed_commits: &[String],
    conflicting_files: &[String],
    source: &str,
    worker_hint: Option<&str>,
) -> Result<(), String> {
    let _lock = acquire_task_lock(task_id).await?;

    let Some(mut task) = read_task(task_id) else {
        return Err(format!("Task not found: {task_id}"));
    };

    apply_supervisor_integration_conflict(
        &mut task,
        desired_status,
        merge_target,
        reviewed_commit,
        reviewed_commits,
        conflicting_files,
        source,
        worker_hint,
    );

    if !write_task(task_id, &task) {
        return Err(format!("Failed to write task {task_id}"));
    }
    reconcile_dependency_states_with_task_lock(task_id).await?;
    Ok(())
}

/// Remove integration-conflict metadata from `task`, clearing conflict-related
/// `activity` and `blockers`. Returns the removed conflict blob.
///
/// Modern conflict-marking preserves `assignee`/`review_owner`, so the
/// restore-from-`previous_worker` branch below is a backwards-compat
/// safety net: legacy task JSON written before that change has nulled
/// fields, and we want existing runs to heal themselves on the next
/// abort-integration / re-review pass without requiring a separate
/// migration step. Once all live runs have rolled past the assignee-
/// preserving writer, this branch becomes a pure no-op.
pub(super) fn apply_integration_conflict_cleanup(
    task: &mut serde_json::Map<String, Value>,
) -> Option<Value> {
    let had_integration_conflict_blocker = task
        .get("blockers")
        .and_then(|value| value.as_str())
        .is_some_and(is_integration_conflict_blocker);
    let conflict = task.remove("integration_conflict");
    if let Some(ref conflict) = conflict {
        let previous_worker = conflict
            .get("previous_worker")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(ref worker) = previous_worker {
            let assignee_empty = task
                .get("assignee")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .is_none_or(str::is_empty);
            if assignee_empty {
                task.insert("assignee".into(), Value::String(worker.clone()));
            }
            let review_owner_empty = task
                .get("review_owner")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .is_none_or(str::is_empty);
            if review_owner_empty {
                task.insert("review_owner".into(), Value::String(worker.clone()));
            }
        }
    }

    if conflict.is_some() || had_integration_conflict_blocker {
        task.remove("activity");
        if had_integration_conflict_blocker {
            task.remove("blockers");
        }
        task.insert(
            "updated_at".into(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
    }
    conflict
}

pub(crate) async fn clear_task_supervisor_integration_conflict(
    task_id: &str,
) -> Result<(), String> {
    let _lock = acquire_task_lock(task_id).await?;
    let Some(mut task) = read_task(task_id) else {
        return Err(format!("Task not found: {task_id}"));
    };

    let had_recovery_marker = task_has_integration_conflict_recovery_marker(&task);
    if apply_integration_conflict_cleanup(&mut task).is_some() || had_recovery_marker {
        if !write_task(task_id, &task) {
            return Err(format!("Failed to write task {task_id}"));
        }
        reconcile_dependency_states_with_task_lock(task_id).await?;
    }

    Ok(())
}

/// Start a cherry-pick of `sha` in worktree `wt`. Returns `Ok(())` on success,
/// or `Err` with the git stderr on failure. Does **not** abort on conflict;
/// the caller decides whether to continue, resolve, or abort.
pub(super) fn start_cherry_pick(wt: &Path, sha: &str) -> Result<(), String> {
    git_run_ok_in(wt, &["cherry-pick", "-x", sha])
}

/// Continue an in-progress cherry-pick in worktree `wt`.
/// - If the cherry-pick produced no changes (empty), skips it.
/// - If the cherry-pick has staged/resolved changes, continues it.
/// - If there are unresolved conflicts, returns an error.
/// - If no cherry-pick is in progress, returns an error.
pub(super) fn continue_cherry_pick(wt: &Path) -> Result<(), String> {
    let cherry_pick_head_path = git_stdout_in(wt, &["rev-parse", "--git-path", "CHERRY_PICK_HEAD"])
        .map_err(|e| format!("Failed to locate CHERRY_PICK_HEAD: {e}"))?;
    let cherry_pick_head_path = PathBuf::from(cherry_pick_head_path);
    let cherry_pick_head_path = if cherry_pick_head_path.is_absolute() {
        cherry_pick_head_path
    } else {
        wt.join(cherry_pick_head_path)
    };
    if !cherry_pick_head_path.exists() {
        return Err("No cherry-pick in progress".to_string());
    }

    let unmerged =
        unmerged_files(wt).map_err(|err| format!("Failed to inspect unmerged files: {err}"))?;
    if !unmerged.is_empty() {
        return Err(format!(
            "Cherry-pick has unresolved conflicts: {}",
            unmerged.join(", ")
        ));
    }

    let has_staged = {
        let output = crate::git_exec::run_git(wt, &["diff", "--cached", "--quiet"])?;
        match output.status.code() {
            Some(0) => false,
            Some(1) => true,
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(format!(
                    "git diff --cached --quiet failed: {}",
                    if stderr.is_empty() {
                        "unknown error"
                    } else {
                        &stderr
                    }
                ));
            }
        }
    };
    if has_staged {
        git_run_ok_in(wt, &["cherry-pick", "--continue"])
    } else {
        git_run_ok_in(wt, &["cherry-pick", "--skip"])
    }
}

/// Verify whether `sha` is already applied (reachable from `branch`) in `wt`.
pub(super) fn verify_applied(wt: &Path, sha: &str, branch: &str) -> Result<bool, String> {
    git_commit_is_ancestor_in(wt, sha, branch)
}

pub(super) fn slugify_branch_component(title: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in title.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if ch.is_ascii_whitespace() || matches!(ch, '-' | '_' | '/' | ':') {
            Some('-')
        } else {
            None
        };

        let Some(normalized) = normalized else {
            continue;
        };

        if normalized == '-' {
            if slug.is_empty() || last_was_dash {
                continue;
            }
            last_was_dash = true;
            slug.push('-');
        } else {
            last_was_dash = false;
            slug.push(normalized);
        }
    }

    slug.trim_matches('-').to_string()
}

pub(super) fn container_branch_prefix(task_type: &str) -> &'static str {
    match task_type {
        "initiative" => "initiative",
        _ => "epic",
    }
}

pub(super) fn default_container_integration_branch(
    task_id: &str,
    title: &str,
    task_type: &str,
) -> String {
    let short_id = task_id
        .strip_prefix("T-")
        .unwrap_or(task_id)
        .to_ascii_lowercase();
    let slug = slugify_branch_component(title);
    let prefix = container_branch_prefix(task_type);
    if slug.is_empty() {
        format!("{prefix}/{short_id}")
    } else {
        format!("{prefix}/{slug}-{short_id}")
    }
}

pub(super) fn default_container_integration_worktree(
    task_id: &str,
    task_type: &str,
) -> Result<PathBuf, String> {
    let worktrees_root = brehon_worktrees_root().ok_or_else(|| {
        "No BREHON_WORKTREE_ROOT or BREHON_ROOT available to allocate container integration worktree."
            .to_string()
    })?;
    Ok(worktrees_root
        .join(container_branch_prefix(task_type))
        .join(task_id))
}

pub(super) async fn remove_container_integration_worktree(path: &Path) -> Result<(), String> {
    let path = resolve_project_path(path).ok_or_else(|| {
        "No project root available to remove container integration worktree.".to_string()
    })?;
    if !path.exists() {
        return Ok(());
    }

    let root = project_root().ok_or_else(|| {
        "No project root available to remove container integration worktree.".to_string()
    })?;
    let git = brehon_git::Git2Operations::open(&root).map_err(|err| {
        format!(
            "Failed to open git repository at '{}': {err}",
            root.display()
        )
    })?;
    git.remove_worktree(&path).await.map_err(|err| {
        format!(
            "Failed to remove container integration worktree '{}': {err}",
            path.display()
        )
    })
}

pub(super) async fn ensure_epic_integration_worktree(
    epic_id: &str,
    integration_branch: &str,
    requested_path: Option<&str>,
    create_branch_if_missing: bool,
    allow_dirty_reuse: bool,
) -> Result<PathBuf, String> {
    ensure_container_integration_worktree(
        epic_id,
        "epic",
        integration_branch,
        requested_path,
        create_branch_if_missing,
        allow_dirty_reuse,
        None,
    )
    .await
}

pub(super) async fn ensure_container_integration_worktree(
    container_id: &str,
    task_type: &str,
    integration_branch: &str,
    requested_path: Option<&str>,
    create_branch_if_missing: bool,
    allow_dirty_reuse: bool,
    base_branch: Option<&str>,
) -> Result<PathBuf, String> {
    let worktree_path = requested_path
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .and_then(|path| resolve_project_path(&path))
        .unwrap_or(default_container_integration_worktree(
            container_id,
            task_type,
        )?);
    let worktree_path =
        ensure_brehon_worktree_path(&worktree_path, "container integration worktree")?;

    if worktree_path.exists() {
        let current_branch = git_stdout_in(&worktree_path, &["branch", "--show-current"]).map_err(
            |err| {
                format!(
                    "{} {} integration_worktree '{}' exists but is not a usable git worktree: {err}",
                    task_type,
                    container_id,
                    worktree_path.display()
                )
            },
        )?;
        if current_branch != integration_branch {
            return Err(format!(
                "{} {} integration_worktree '{}' is on branch '{}' instead of '{}'. \
                 Repair or recreate the container worktree before continuing.",
                task_type,
                container_id,
                worktree_path.display(),
                current_branch,
                integration_branch
            ));
        }
        let reuse_issues = existing_integration_worktree_reuse_issues(&worktree_path)?;
        if !allow_dirty_reuse && !reuse_issues.is_empty() {
            return Err(format!(
                "{} {} integration_worktree '{}' cannot be reused because it has {}. \
                 Resolve it with abort-integration or recreate the worktree; \
                 if you intend to discard the previous integration attempt, rerun with force=true.",
                task_type,
                container_id,
                worktree_path.display(),
                reuse_issues.join("; ")
            ));
        }
        return Ok(worktree_path);
    }

    let root = project_root().ok_or_else(|| {
        "No project root available to provision container integration worktree.".to_string()
    })?;
    ensure_brehon_ignored_in_repo(&root)?;
    let git = brehon_git::Git2Operations::open(&root).map_err(|err| {
        format!(
            "Failed to open git repository at '{}': {err}",
            root.display()
        )
    })?;

    if !git_branch_exists_in(&root, integration_branch)? {
        if !create_branch_if_missing {
            return Err(format!(
                "{} {} has integration_branch '{}' but the branch does not exist. \
                 Recreate the container integration branch before continuing.",
                task_type, container_id, integration_branch
            ));
        }
        let base_branch = base_branch
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or(detect_default_branch_in(&root)?);
        git.create_branch(integration_branch, Some(&base_branch))
            .await
            .map_err(|err| {
                format!(
                    "Failed to create {} integration branch '{}' from '{}': {err}",
                    task_type, integration_branch, base_branch
                )
            })?;
    }

    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create parent directory '{}' for container integration worktree: {err}",
                parent.display()
            )
        })?;
    }

    git.create_worktree(integration_branch, &worktree_path)
        .await
        .map_err(|err| {
            format!(
                "Failed to create {} integration worktree for {} on branch '{}' at '{}': {err}",
                task_type,
                container_id,
                integration_branch,
                worktree_path.display()
            )
        })?;

    Ok(worktree_path)
}

fn existing_integration_worktree_reuse_issues(worktree_path: &Path) -> Result<Vec<String>, String> {
    let mut issues = Vec::new();

    for (git_path, description) in [
        ("CHERRY_PICK_HEAD", "stale cherry-pick state"),
        ("MERGE_HEAD", "stale merge state"),
        ("REBASE_HEAD", "stale rebase state"),
    ] {
        let state_path = git_stdout_in(worktree_path, &["rev-parse", "--git-path", git_path])
            .map_err(|err| {
                format!(
                    "Failed to locate {git_path} in integration worktree '{}': {err}",
                    worktree_path.display()
                )
            })?;
        let state_path = PathBuf::from(state_path);
        let state_path = if state_path.is_absolute() {
            state_path
        } else {
            worktree_path.join(state_path)
        };
        if state_path.exists() {
            issues.push(format!("{description} ({git_path} present)"));
        }
    }

    let unmerged = unmerged_files(worktree_path)
        .map_err(|err| format!("Failed to inspect unmerged files: {err}"))?;
    if !unmerged.is_empty() {
        issues.push(format!("unmerged files: {}", unmerged.join(", ")));
    }

    let status = git_status_porcelain_in(worktree_path)?;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut non_brehon_untracked = Vec::new();
    for line in status.lines().filter(|line| !line.trim().is_empty()) {
        let path = line.get(3..).unwrap_or("").trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        if line.starts_with("?? ") {
            if !path.is_empty() && !path.starts_with(".brehon/") && path != ".brehon" {
                non_brehon_untracked.push(path.to_string());
            }
            continue;
        }

        let staged_status = line.chars().next().unwrap_or(' ');
        let unstaged_status = line.chars().nth(1).unwrap_or(' ');
        if staged_status != ' ' && staged_status != '?' {
            staged.push(path.to_string());
        }
        if unstaged_status != ' ' && unstaged_status != '?' {
            unstaged.push(path.to_string());
        }
    }

    if !staged.is_empty() {
        issues.push(format!("staged changes: {}", staged.join(", ")));
    }
    if !unstaged.is_empty() {
        issues.push(format!("unstaged changes: {}", unstaged.join(", ")));
    }
    if !non_brehon_untracked.is_empty() {
        issues.push(format!(
            "untracked files outside .brehon/: {}",
            non_brehon_untracked.join(", ")
        ));
    }

    Ok(issues)
}

// --- Helper functions used by tool.rs for epic/container close flow ---

/// Check if all direct children of a parent task are terminal.
/// Returns (total_children, closed_children, all_closed).
pub(super) fn check_child_completion(parent_id: &str) -> (usize, usize, bool) {
    let all = read_all_tasks();
    let children = super::lifecycle::direct_children(&all, parent_id);
    let total = children.len();
    let closed = children
        .iter()
        .filter(|t| {
            t.get("status")
                .and_then(|v| v.as_str())
                .is_some_and(is_terminal_task_status)
        })
        .count();
    (total, closed, total > 0 && total == closed)
}

/// Check if all subtasks of a given epic are closed.
/// Returns (total_subtasks, closed_subtasks, all_closed).
pub(super) fn check_epic_completion(epic_id: &str) -> (usize, usize, bool) {
    check_child_completion(epic_id)
}

/// Check if all subtasks of a feature epic (with integration_branch) are integrated.
/// Returns (total_subtasks, integrated_count, missing_list) where missing_list contains
/// subtask IDs that are not yet integrated.
pub(super) fn check_epic_integration_status(epic_id: &str) -> (usize, usize, Vec<String>) {
    let all = read_all_tasks();
    let subtasks: Vec<_> = all
        .iter()
        .filter(|t| t.get("parent_id").and_then(|v| v.as_str()) == Some(epic_id))
        .collect();
    let total = subtasks.len();
    let integrated = subtasks
        .iter()
        .filter(|t| {
            t.get("integration_status")
                .and_then(|v| v.as_str())
                .map(|s| s == "integrated" || s == "not_applicable")
                .unwrap_or(false)
        })
        .count();
    let missing: Vec<String> = subtasks
        .iter()
        .filter(|t| {
            let status = t
                .get("integration_status")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            status != "integrated" && status != "not_applicable"
        })
        .filter_map(|t| {
            t.get("task_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    (total, integrated, missing)
}

pub(super) fn check_initiative_epic_integration_status(
    initiative_id: &str,
) -> (usize, usize, Vec<String>) {
    let all = read_all_tasks();
    let epics: Vec<_> = all
        .iter()
        .filter(|task| task.get("parent_id").and_then(|v| v.as_str()) == Some(initiative_id))
        .collect();
    let total = epics.len();
    let integrated = epics
        .iter()
        .filter(|task| {
            let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "merged" {
                return true;
            }
            if task
                .get("integration_status")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == "integrated" || value == "not_applicable")
            {
                return true;
            }
            task.get("integration_branch")
                .and_then(|v| v.as_str())
                .is_none_or(|branch| branch.trim().is_empty())
                && is_terminal_task_status(status)
        })
        .count();
    let missing = epics
        .iter()
        .filter(|task| {
            let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "merged" {
                return false;
            }
            if task
                .get("integration_status")
                .and_then(|v| v.as_str())
                .is_some_and(|value| value == "integrated" || value == "not_applicable")
            {
                return false;
            }
            !(task
                .get("integration_branch")
                .and_then(|v| v.as_str())
                .is_none_or(|branch| branch.trim().is_empty())
                && is_terminal_task_status(status))
        })
        .filter_map(|task| {
            task.get("task_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    (total, integrated, missing)
}

pub(super) fn read_parent_task(
    task: &serde_json::Map<String, Value>,
) -> Option<serde_json::Map<String, Value>> {
    let parent_id = task.get("parent_id").and_then(|v| v.as_str())?;
    read_task(parent_id)
}

pub(super) fn container_base_branch_for_parent(
    task_type: &str,
    parent_task: Option<&serde_json::Map<String, Value>>,
) -> Result<String, String> {
    if is_initiative(task_type) {
        return detect_default_branch();
    }

    if is_epic(task_type) {
        if let Some(parent) = parent_task {
            let parent_type = parent
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            if is_initiative(parent_type) {
                if let Some(branch) = parent
                    .get("integration_branch")
                    .and_then(|v| v.as_str())
                    .filter(|value| !value.is_empty())
                {
                    return Ok(branch.to_string());
                }
                let parent_id = parent
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("initiative");
                return Err(format!(
                    "Initiative {parent_id} is missing integration_branch. \
                     Reconcile the initiative hierarchy before continuing so child epics do not fall back to the default branch."
                ));
            }
        }
    }

    detect_default_branch()
}

pub(super) fn container_base_branch_for_task(
    task: &serde_json::Map<String, Value>,
) -> Result<String, String> {
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    let parent = read_parent_task(task);
    container_base_branch_for_parent(task_type, parent.as_ref())
}

pub(super) async fn container_target_worktree_for_task(
    task: &serde_json::Map<String, Value>,
    target_branch: &str,
) -> Result<PathBuf, String> {
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");

    if is_epic(task_type) {
        if let Some(parent) = read_parent_task(task) {
            let parent_type = parent
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            if is_initiative(parent_type) {
                let parent_id = parent
                    .get("task_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Initiative parent is missing task_id.".to_string())?;
                let Some(parent_branch) = parent
                    .get("integration_branch")
                    .and_then(|v| v.as_str())
                    .filter(|value| !value.is_empty())
                else {
                    return Err(format!(
                        "Initiative {parent_id} is missing integration_branch. \
                         Reconcile the initiative hierarchy before closing child epic {}.",
                        task.get("task_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                    ));
                };

                if parent_branch == target_branch {
                    return ensure_container_integration_worktree(
                        parent_id,
                        "initiative",
                        target_branch,
                        parent
                            .get("integration_worktree")
                            .and_then(|v| v.as_str())
                            .filter(|value| !value.is_empty()),
                        false,
                        false,
                        Some(&detect_default_branch()?),
                    )
                    .await;
                }
            }
        }
    }

    project_root().ok_or_else(|| "No project root available for target merge.".to_string())
}

/// Verify container branch is ready for merge into its base branch.
/// Returns Ok(branch_name) if the container branch exists and is a descendant of the base branch.
pub(super) fn verify_container_branch_ready(
    container_id: &str,
    container_type: &str,
    integration_branch: &str,
    integration_worktree: &Path,
    base_branch: &str,
) -> Result<String, String> {
    let workspace = project_root().ok_or_else(|| {
        "No project root available for container branch verification.".to_string()
    })?;

    if !git_branch_exists_in(&workspace, integration_branch)? {
        return Err(format!(
            "{} {} has integration_branch '{}' but the branch does not exist. \
             Create the branch first before closing the container.",
            container_type, container_id, integration_branch
        ));
    }

    let current_branch = git_stdout_in(integration_worktree, &["branch", "--show-current"])
        .map_err(|err| {
            format!(
                "{} {} integration_worktree '{}' is not usable: {}",
                container_type,
                container_id,
                integration_worktree.display(),
                err
            )
        })?;
    if current_branch != integration_branch {
        return Err(format!(
            "{} {} integration_worktree '{}' is on '{}' instead of '{}'. \
             Repair the container worktree before closing the container.",
            container_type,
            container_id,
            integration_worktree.display(),
            current_branch,
            integration_branch
        ));
    }

    let merge_base_output = crate::git_exec::run_git(
        &workspace,
        &[
            "merge-base",
            "--is-ancestor",
            base_branch,
            integration_branch,
        ],
    )?;

    if !merge_base_output.status.success() {
        return Err(format!(
            "{} branch '{}' is not a descendant of '{}'. \
             The branch may have been force-pushed or rebased. \
             Merge '{}' into the '{}' branch first, or rebase the '{}' branch.",
            container_type,
            integration_branch,
            base_branch,
            base_branch,
            container_type,
            container_type
        ));
    }

    Ok(integration_branch.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContainerMergeStrategy {
    Merge,
    Squash,
}

impl ContainerMergeStrategy {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContainerMergeOutcome {
    pub(super) commit: String,
    pub(super) strategy: ContainerMergeStrategy,
    pub(super) squash_source_tip: Option<String>,
}

#[cfg(test)]
pub(super) fn merge_container_branch_into_target(
    container_id: &str,
    container_type: &str,
    integration_branch: &str,
    target_branch: &str,
    target_worktree: &Path,
) -> Result<String, String> {
    merge_container_branch_into_target_with_strategy(
        container_id,
        None,
        container_type,
        integration_branch,
        target_branch,
        target_worktree,
        ContainerMergeStrategy::Merge,
    )
    .map(|outcome| outcome.commit)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn merge_container_branch_into_target_with_strategy(
    container_id: &str,
    container_title: Option<&str>,
    container_type: &str,
    integration_branch: &str,
    target_branch: &str,
    target_worktree: &Path,
    strategy: ContainerMergeStrategy,
) -> Result<ContainerMergeOutcome, String> {
    let current_branch =
        git_stdout_in(target_worktree, &["branch", "--show-current"]).map_err(|err| {
            format!(
                "Failed to read current branch in target worktree '{}': {err}",
                target_worktree.display()
            )
        })?;
    if current_branch != target_branch {
        return Err(format!(
            "Target worktree '{}' is on '{}' instead of '{}'. Repair the target integration worktree before merging {} {}.",
            target_worktree.display(),
            current_branch,
            target_branch,
            container_type,
            container_id
        ));
    }

    let target_head_before_merge = git_stdout_in(target_worktree, &["rev-parse", "HEAD"])?;
    let dirty = git_status_porcelain_in(target_worktree)?;
    let dirty_entries = non_brehon_status_entries(&dirty);
    if !dirty_entries.is_empty() {
        return Err(format!(
            "Target worktree '{}' is dirty ({}). Clean it before merging {} {} into '{}'.",
            target_worktree.display(),
            dirty_entries.join(", "),
            container_type,
            container_id,
            target_branch
        ));
    }

    let squash_source_tip = if strategy == ContainerMergeStrategy::Squash {
        Some(git_stdout_in(
            target_worktree,
            &["rev-parse", integration_branch],
        )?)
    } else {
        None
    };

    if strategy == ContainerMergeStrategy::Squash {
        let squash_result = crate::git_exec::run_git_allow_protected_branch_commit(
            target_worktree,
            &["merge", "--squash", integration_branch],
        )?;

        if !squash_result.status.success() {
            reset_target_worktree_after_failed_merge(target_worktree, &target_head_before_merge);
            return Err(format!(
                "Failed to squash merge {} branch '{}' into '{}'. Merge conflict or error: {}",
                container_type,
                integration_branch,
                target_branch,
                String::from_utf8_lossy(&squash_result.stderr)
            ));
        }

        let commit_message =
            squash_commit_message(container_id, container_title, container_type, target_branch);
        let commit_result = crate::git_exec::run_git_allow_protected_branch_commit(
            target_worktree,
            &["commit", "--allow-empty", "-m", &commit_message],
        )?;

        if !commit_result.status.success() {
            reset_target_worktree_after_failed_merge(target_worktree, &target_head_before_merge);
            return Err(format!(
                "Failed to commit squash merge of {} branch '{}' into '{}': {}",
                container_type,
                integration_branch,
                target_branch,
                String::from_utf8_lossy(&commit_result.stderr)
            ));
        }

        let merge_commit = git_stdout_in(target_worktree, &["rev-parse", "HEAD"])?;
        return Ok(ContainerMergeOutcome {
            commit: merge_commit,
            strategy,
            squash_source_tip,
        });
    }

    let merge_result = crate::git_exec::run_git_allow_protected_branch_commit(
        target_worktree,
        &[
            "merge",
            "--no-ff",
            integration_branch,
            "-m",
            &format!("Merge {container_type} {container_id} into {target_branch}"),
        ],
    )?;

    if !merge_result.status.success() {
        let _ = crate::git_exec::run_git(target_worktree, &["merge", "--abort"]);
        reset_target_worktree_after_failed_merge(target_worktree, &target_head_before_merge);
        return Err(format!(
            "Failed to merge {} branch '{}' into '{}'. Merge conflict or error: {}",
            container_type,
            integration_branch,
            target_branch,
            String::from_utf8_lossy(&merge_result.stderr)
        ));
    }

    let merge_commit = git_stdout_in(target_worktree, &["rev-parse", "HEAD"])?;

    Ok(ContainerMergeOutcome {
        commit: merge_commit,
        strategy,
        squash_source_tip,
    })
}

fn squash_commit_message(
    container_id: &str,
    container_title: Option<&str>,
    container_type: &str,
    target_branch: &str,
) -> String {
    if let Some(title) = container_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        format!("Merge {container_type} {container_id}: {title}")
    } else {
        format!("Merge {container_type} {container_id} into {target_branch}")
    }
}

fn reset_target_worktree_after_failed_merge(target_worktree: &Path, target_head: &str) {
    let _ = crate::git_exec::run_git(target_worktree, &["merge", "--abort"]);
    let _ = crate::git_exec::run_git(target_worktree, &["reset", "--hard", target_head]);
}

#[cfg(test)]
mod tests {
    use super::{
        continue_cherry_pick, ensure_container_integration_worktree,
        existing_integration_worktree_reuse_issues, merge_container_branch_into_target,
        merge_container_branch_into_target_with_strategy, ContainerMergeStrategy,
    };
    use crate::tools::TEST_ENV_LOCK;
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Command;

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(root: &Path) {
        run_git(root, &["init", "-b", "main"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test User"]);
        std::fs::write(root.join("README.md"), "seed\n").unwrap();
        run_git(root, &["add", "README.md"]);
        run_git(root, &["commit", "-m", "seed"]);
    }

    fn git_path(root: &Path, git_path: &str) -> std::path::PathBuf {
        let path = run_git(root, &["rev-parse", "--git-path", git_path]);
        let path = std::path::PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    }

    fn owned_worktree_path(root: &Path, name: &str) -> std::path::PathBuf {
        let dir = root.join(".brehon").join("worktrees");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    struct ScopedEnv {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl ScopedEnv {
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let mut saved = Vec::with_capacity(vars.len());
            for (key, value) in vars {
                saved.push((*key, std::env::var_os(key)));
                std::env::set_var(key, value);
            }
            Self { saved }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (key, value) in self.saved.iter().rev() {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn scoped_brehon_root(root: &Path) -> ScopedEnv {
        ScopedEnv::set(&[
            ("BREHON_ROOT", root.join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", ""),
            ("BREHON_WORKSPACE_ROOT", ""),
        ])
    }

    #[cfg(unix)]
    fn install_bypass_required_hook(root: &Path, hook_name: &str) {
        use std::os::unix::fs::PermissionsExt;

        let hook = git_path(root, &format!("hooks/{hook_name}"));
        if let Some(parent) = hook.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            &hook,
            r#"#!/bin/sh
if [ "${BREHON_ALLOW_PROTECTED_BRANCH_COMMIT:-}" != "1" ]; then
  echo protected bypass missing >&2
  exit 1
fi
if [ -z "${BREHON_PROTECTED_BRANCH_BYPASS_TOKEN:-}" ]; then
  echo protected bypass token missing >&2
  exit 1
fi
brehon_git_common_dir="$(git rev-parse --git-common-dir 2>/dev/null || true)"
case "$brehon_git_common_dir" in
  /*) ;;
  *) brehon_git_common_dir="$(git rev-parse --show-toplevel)/$brehon_git_common_dir" ;;
esac
if [ ! -f "$brehon_git_common_dir/brehon/protected-branch-bypass/$BREHON_PROTECTED_BRANCH_BYPASS_TOKEN" ]; then
  echo protected bypass lease missing >&2
  exit 1
fi
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn merge_container_branch_into_target_uses_protected_branch_bypass_env() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        run_git(root.path(), &["checkout", "-b", "initiative/test"]);
        std::fs::write(root.path().join("initiative.txt"), "initiative\n").unwrap();
        run_git(root.path(), &["add", "initiative.txt"]);
        run_git(root.path(), &["commit", "-m", "initiative work"]);
        run_git(root.path(), &["checkout", "main"]);

        install_bypass_required_hook(root.path(), "pre-merge-commit");
        install_bypass_required_hook(root.path(), "commit-msg");
        install_bypass_required_hook(root.path(), "reference-transaction");

        let merge_commit = merge_container_branch_into_target(
            "I-1",
            "initiative",
            "initiative/test",
            "main",
            root.path(),
        )
        .expect("controlled merge should pass the protected branch hook");

        assert_eq!(run_git(root.path(), &["rev-parse", "HEAD"]), merge_commit);
        assert_eq!(
            run_git(root.path(), &["log", "-1", "--format=%s"]),
            "Merge initiative I-1 into main"
        );
    }

    #[cfg(unix)]
    #[test]
    fn squash_merge_container_branch_uses_protected_branch_bypass_env_and_records_source_tip() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        run_git(root.path(), &["checkout", "-b", "initiative/test"]);
        std::fs::write(root.path().join("initiative.txt"), "initiative\n").unwrap();
        run_git(root.path(), &["add", "initiative.txt"]);
        run_git(root.path(), &["commit", "-m", "initiative work"]);
        let source_tip = run_git(root.path(), &["rev-parse", "HEAD"]);
        run_git(root.path(), &["checkout", "main"]);

        install_bypass_required_hook(root.path(), "commit-msg");
        install_bypass_required_hook(root.path(), "reference-transaction");

        let outcome = merge_container_branch_into_target_with_strategy(
            "I-1",
            Some("Test Program"),
            "initiative",
            "initiative/test",
            "main",
            root.path(),
            ContainerMergeStrategy::Squash,
        )
        .expect("controlled squash merge should pass the protected branch hook");

        assert_eq!(outcome.strategy, ContainerMergeStrategy::Squash);
        assert_eq!(
            outcome.squash_source_tip.as_deref(),
            Some(source_tip.as_str())
        );
        assert_eq!(run_git(root.path(), &["rev-parse", "HEAD"]), outcome.commit);
        assert_eq!(
            run_git(root.path(), &["log", "-1", "--format=%s"]),
            "Merge initiative I-1: Test Program"
        );
    }

    #[test]
    fn squash_merge_conflict_resets_target_worktree_to_pre_merge_head() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        std::fs::write(root.path().join("shared.txt"), "base\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "add shared file"]);

        run_git(root.path(), &["checkout", "-b", "initiative/test"]);
        std::fs::write(root.path().join("shared.txt"), "initiative change\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "initiative change"]);

        run_git(root.path(), &["checkout", "main"]);
        std::fs::write(root.path().join("shared.txt"), "main change\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "main change"]);
        let target_head = run_git(root.path(), &["rev-parse", "HEAD"]);

        let err = merge_container_branch_into_target_with_strategy(
            "I-1",
            Some("Test Program"),
            "initiative",
            "initiative/test",
            "main",
            root.path(),
            ContainerMergeStrategy::Squash,
        )
        .expect_err("conflicting squash merge should fail");

        assert!(
            err.contains("Failed to squash merge initiative branch 'initiative/test'"),
            "unexpected error: {err}"
        );
        assert_eq!(run_git(root.path(), &["rev-parse", "HEAD"]), target_head);
        assert_eq!(run_git(root.path(), &["status", "--porcelain"]), "");
        assert_eq!(
            std::fs::read_to_string(root.path().join("shared.txt")).unwrap(),
            "main change\n"
        );
    }

    #[test]
    fn continue_cherry_pick_skips_empty_pick_despite_untracked_files() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        // Create a worker branch with a commit that adds src.txt
        run_git(root.path(), &["checkout", "-b", "worker/task"]);
        std::fs::write(root.path().join("src.txt"), "shared implementation\n").unwrap();
        run_git(root.path(), &["add", "src.txt"]);
        run_git(root.path(), &["commit", "-m", "worker implementation"]);
        let reviewed_commit = run_git(root.path(), &["rev-parse", "HEAD"]);

        // Return to main and apply the same change so the cherry-pick is empty
        run_git(root.path(), &["checkout", "main"]);
        std::fs::write(root.path().join("src.txt"), "shared implementation\n").unwrap();
        run_git(root.path(), &["add", "src.txt"]);
        run_git(
            root.path(),
            &["commit", "-m", "main already has implementation"],
        );

        // Start the cherry-pick; it will fail because the pick is empty
        let output = Command::new("git")
            .args(["cherry-pick", &reviewed_commit])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());

        // Add an untracked file — this must not confuse continue_cherry_pick
        std::fs::write(root.path().join("scratch.txt"), "untracked artifact\n").unwrap();

        // continue_cherry_pick should detect the empty pick and skip it
        let result = continue_cherry_pick(root.path());
        assert!(
            result.is_ok(),
            "continue_cherry_pick should skip the empty pick even with untracked files, got: {:?}",
            result
        );

        // After skipping, the cherry-pick state should be cleared
        assert!(
            !git_path(root.path(), "CHERRY_PICK_HEAD").exists(),
            "CHERRY_PICK_HEAD should be gone after skip"
        );
    }

    #[test]
    fn continue_cherry_pick_errors_when_conflicts_remain_unresolved() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        std::fs::write(root.path().join("shared.txt"), "base\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "add shared file"]);

        run_git(root.path(), &["checkout", "-b", "worker/task"]);
        std::fs::write(root.path().join("shared.txt"), "worker change\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "worker change"]);
        let reviewed_commit = run_git(root.path(), &["rev-parse", "HEAD"]);

        run_git(root.path(), &["checkout", "main"]);
        std::fs::write(root.path().join("shared.txt"), "main change\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "main change"]);

        let output = Command::new("git")
            .args(["cherry-pick", &reviewed_commit])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            git_path(root.path(), "CHERRY_PICK_HEAD").exists(),
            "CHERRY_PICK_HEAD should exist while resolving the cherry-pick"
        );

        let result = continue_cherry_pick(root.path());
        let err =
            result.expect_err("continue_cherry_pick should error when unresolved conflicts remain");
        assert!(
            err.contains("Cherry-pick has unresolved conflicts"),
            "expected unresolved-conflicts error, got: {err:?}"
        );
        assert!(
            err.contains("shared.txt"),
            "expected conflict list to include shared.txt, got: {err:?}"
        );
        assert!(
            std::fs::read_to_string(root.path().join("shared.txt"))
                .unwrap()
                .contains("<<<<<<<"),
            "shared.txt should still contain conflict markers while conflicts are unresolved"
        );
        assert!(
            git_path(root.path(), "CHERRY_PICK_HEAD").exists(),
            "CHERRY_PICK_HEAD should remain while conflicts are unresolved"
        );
    }

    #[test]
    fn continue_cherry_pick_continues_when_resolution_has_staged_changes() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        std::fs::write(root.path().join("shared.txt"), "base\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "add shared file"]);

        run_git(root.path(), &["checkout", "-b", "worker/task"]);
        std::fs::write(root.path().join("shared.txt"), "worker change\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "worker change"]);
        let reviewed_commit = run_git(root.path(), &["rev-parse", "HEAD"]);

        run_git(root.path(), &["checkout", "main"]);
        std::fs::write(root.path().join("shared.txt"), "main change\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);
        run_git(root.path(), &["commit", "-m", "main change"]);

        let output = Command::new("git")
            .args(["cherry-pick", &reviewed_commit])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            git_path(root.path(), "CHERRY_PICK_HEAD").exists(),
            "CHERRY_PICK_HEAD should exist while resolving the cherry-pick"
        );

        std::fs::write(root.path().join("shared.txt"), "resolved change\n").unwrap();
        run_git(root.path(), &["add", "shared.txt"]);

        let result = continue_cherry_pick(root.path());
        assert!(
            result.is_ok(),
            "continue_cherry_pick should continue when staged changes are present, got: {:?}",
            result
        );

        assert!(
            !git_path(root.path(), "CHERRY_PICK_HEAD").exists(),
            "CHERRY_PICK_HEAD should be gone after continue"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("shared.txt")).unwrap(),
            "resolved change\n"
        );
        assert_eq!(
            run_git(root.path(), &["log", "-1", "--format=%s"]),
            "worker change"
        );
    }

    #[tokio::test]
    async fn ensure_container_integration_worktree_rejects_dirty_existing_worktree() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = scoped_brehon_root(root.path());
        init_repo(root.path());

        let worktree_path = owned_worktree_path(root.path(), "epic-worktree");
        run_git(
            root.path(),
            &[
                "worktree",
                "add",
                "-b",
                "epic/test-integration",
                worktree_path.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(worktree_path.join("scratch.txt"), "dirty\n").unwrap();
        std::fs::create_dir_all(worktree_path.join(".brehon")).unwrap();
        std::fs::write(worktree_path.join(".brehon/allowed.txt"), "metadata\n").unwrap();

        let err = ensure_container_integration_worktree(
            "T-epic",
            "epic",
            "epic/test-integration",
            Some(worktree_path.to_str().unwrap()),
            false,
            false,
            None,
        )
        .await
        .expect_err("dirty worktree should be rejected");

        assert!(
            err.contains("cannot be reused"),
            "error should explain that dirty worktree reuse is rejected: {err}"
        );
        assert!(
            err.contains("untracked files outside .brehon/: scratch.txt"),
            "error should report the non-.brehon untracked file: {err}"
        );
        assert!(
            err.contains("abort-integration"),
            "error should direct callers to abort-integration: {err}"
        );
        assert!(
            err.contains("force=true"),
            "error should direct callers to the force=true recovery path: {err}"
        );
    }

    #[tokio::test]
    async fn ensure_container_integration_worktree_rejects_external_requested_path() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = scoped_brehon_root(root.path());
        init_repo(root.path());

        let worktree_path = root.path().join("external-worktree");
        run_git(
            root.path(),
            &[
                "worktree",
                "add",
                "-b",
                "epic/test-integration",
                worktree_path.to_str().unwrap(),
                "HEAD",
            ],
        );

        let err = ensure_container_integration_worktree(
            "T-epic",
            "epic",
            "epic/test-integration",
            Some(worktree_path.to_str().unwrap()),
            false,
            false,
            None,
        )
        .await
        .expect_err("external requested worktree should be rejected");

        assert!(
            err.contains("outside Brehon-owned worktrees"),
            "error should report owned-worktree guard: {err}"
        );
    }

    #[tokio::test]
    async fn ensure_container_integration_worktree_rejects_unstaged_tracked_changes() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = scoped_brehon_root(root.path());
        init_repo(root.path());

        let worktree_path = owned_worktree_path(root.path(), "epic-worktree");
        run_git(
            root.path(),
            &[
                "worktree",
                "add",
                "-b",
                "epic/test-integration",
                worktree_path.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(worktree_path.join("README.md"), "modified but unstaged\n").unwrap();

        let err = ensure_container_integration_worktree(
            "T-epic",
            "epic",
            "epic/test-integration",
            Some(worktree_path.to_str().unwrap()),
            false,
            false,
            None,
        )
        .await
        .expect_err("worktree with unstaged tracked changes should be rejected");

        assert!(
            err.contains("cannot be reused"),
            "error should explain that dirty worktree reuse is rejected: {err}"
        );
        assert!(
            err.contains("unstaged changes: README.md"),
            "error should report the unstaged tracked change: {err}"
        );
    }

    #[tokio::test]
    async fn ensure_container_integration_worktree_rejects_merge_and_rebase_state() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = scoped_brehon_root(root.path());
        init_repo(root.path());

        let worktree_path = owned_worktree_path(root.path(), "epic-worktree");
        run_git(
            root.path(),
            &[
                "worktree",
                "add",
                "-b",
                "epic/test-integration",
                worktree_path.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(git_path(&worktree_path, "MERGE_HEAD"), "deadbeef\n").unwrap();
        std::fs::write(git_path(&worktree_path, "REBASE_HEAD"), "cafebabe\n").unwrap();

        let err = ensure_container_integration_worktree(
            "T-epic",
            "epic",
            "epic/test-integration",
            Some(worktree_path.to_str().unwrap()),
            false,
            false,
            None,
        )
        .await
        .expect_err("worktree with merge or rebase state should be rejected");

        assert!(
            err.contains("stale merge state (MERGE_HEAD present)"),
            "error should report stale merge state: {err}"
        );
        assert!(
            err.contains("stale rebase state (REBASE_HEAD present)"),
            "error should report stale rebase state: {err}"
        );
    }

    #[test]
    fn existing_integration_worktree_reuse_issues_errors_when_unmerged_probe_fails() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let worktree_path = root.path().join("epic-worktree");
        run_git(
            root.path(),
            &[
                "worktree",
                "add",
                "-b",
                "epic/test-integration",
                worktree_path.to_str().unwrap(),
                "HEAD",
            ],
        );

        let index_path = git_path(&worktree_path, "index");
        let backup_path = root.path().join("index.backup");
        std::fs::rename(&index_path, &backup_path).unwrap();
        std::fs::create_dir(&index_path).unwrap();

        let result = existing_integration_worktree_reuse_issues(&worktree_path);

        std::fs::remove_dir(&index_path).unwrap();
        std::fs::rename(&backup_path, &index_path).unwrap();

        let err = result.expect_err("broken git diff probe should be surfaced as an error");
        assert!(
            err.contains("Failed to inspect unmerged files"),
            "error should propagate unmerged-file probe failures: {err}"
        );
    }
}
