//! Worktree operations: discovery, state checking, archival, removal.

use std::path::{Path, PathBuf};

use super::paths::{brehon_root, worktrees_root};

pub(super) fn candidate_worker_worktree_paths(worker_name: &str) -> Vec<PathBuf> {
    let Some(worktrees_dir) = worktrees_root() else {
        return Vec::new();
    };
    let mut candidates = Vec::new();

    let legacy = worktrees_dir.join(worker_name);
    if legacy.is_dir() {
        candidates.push(legacy);
    }

    let runs_dir = worktrees_dir.join("runs");
    if let Ok(run_entries) = std::fs::read_dir(&runs_dir) {
        for run_entry in run_entries.flatten() {
            let run_path = run_entry.path();
            if !run_path.is_dir() {
                continue;
            }
            let candidate = run_path.join(worker_name);
            if candidate.is_dir() {
                candidates.push(candidate);
            }
        }
    }

    candidates
}

pub(super) fn find_worktree_by_worker(
    worker_name: &str,
) -> Result<Option<(git2::Repository, PathBuf)>, String> {
    let root = match brehon_root() {
        Some(r) => r,
        None => return Ok(None), // No brehon root = no worktrees to clean
    };

    let repo_path = match root.parent() {
        Some(p) => p,
        None => return Ok(None), // Can't find repo = no worktrees to clean
    };

    let repo = match git2::Repository::open(repo_path) {
        Ok(r) => r,
        Err(_) => return Ok(None), // Can't open repo = no worktrees (test env)
    };

    let Some(worktrees_dir) = worktrees_root() else {
        return Ok(None);
    };
    if !worktrees_dir.exists() {
        return Ok(None);
    }

    let matches = candidate_worker_worktree_paths(worker_name);

    // Reject ambiguous matches
    if matches.len() > 1 {
        let match_list: Vec<_> = matches.iter().map(|p| p.display().to_string()).collect();
        return Err(format!(
            "Ambiguous worktree match: found {} candidates for worker '{}': {}",
            matches.len(),
            worker_name,
            match_list.join(", ")
        ));
    }

    if matches.is_empty() {
        return Ok(None);
    }

    Ok(Some((repo, matches.into_iter().next().unwrap())))
}

pub(super) fn check_worktree_state_with_git2(
    repo: &git2::Repository,
    worktree_path: &Path,
) -> Result<brehon_git::WorktreeStateCheck, String> {
    use brehon_git::WorktreeOps;

    let ops = WorktreeOps::new(repo);
    ops.worktree_state_check(worktree_path)
        .map_err(|e| format!("worktree state check failed: {}", e))
}

pub(super) fn archive_worktree_with_git2(
    repo: &git2::Repository,
    worktree_path: &Path,
    worker_name: &str,
    task_id: &str,
    reason: &str,
) -> Result<String, String> {
    use brehon_git::WorktreeOps;

    let archive_base = worktrees_root()
        .ok_or_else(|| "failed to find Brehon worktrees root".to_string())?
        .join("_archived");

    let ops = WorktreeOps::new(repo);
    let report = ops
        .archive_worktree(worktree_path, &archive_base, task_id)
        .map_err(|e| format!("failed to archive worktree: {}", e))?;

    let archive_name = report
        .archived_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    tracing::info!(
        "Archived worktree for {} to {} (reason: {})",
        worker_name,
        archive_name,
        reason
    );

    Ok(report.archived_path.to_string_lossy().to_string())
}

pub(super) fn remove_worktree_with_git2(
    repo: &git2::Repository,
    worktree_path: &Path,
) -> Result<(), String> {
    use brehon_git::WorktreeOps;

    let ops = WorktreeOps::new(repo);
    ops.remove_worktree(worktree_path)
        .map_err(|e| format!("failed to remove worktree: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct ScopedEnv {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl ScopedEnv {
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let mut saved = Vec::with_capacity(vars.len());
            for (key, value) in vars {
                saved.push((*key, std::env::var_os(key)));
                if value.is_empty() {
                    std::env::remove_var(key);
                } else {
                    std::env::set_var(key, value);
                }
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

    #[test]
    fn candidate_worker_worktree_paths_honors_external_worktree_root() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let brehon = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let worker = external.path().join("runs/run-1/worker-1");
        std::fs::create_dir_all(&worker).unwrap();
        let legacy = brehon.path().join("worktrees/runs/run-1/worker-1");
        std::fs::create_dir_all(&legacy).unwrap();
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", brehon.path().to_str().unwrap()),
            ("BREHON_WORKTREE_ROOT", external.path().to_str().unwrap()),
        ]);

        assert_eq!(candidate_worker_worktree_paths("worker-1"), vec![worker]);
    }
}
