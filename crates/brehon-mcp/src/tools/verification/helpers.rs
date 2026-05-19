use std::path::PathBuf;

pub(crate) fn brehon_root() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

pub(crate) fn workspace_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("BREHON_WORKSPACE_ROOT") {
        let root = root.trim();
        if !root.is_empty() {
            return Some(PathBuf::from(root));
        }
    }

    let brehon_root = brehon_root()?;
    (brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon"))
        .then(|| brehon_root.parent().map(PathBuf::from))
        .flatten()
}

pub(crate) fn git_output(args: &[&str]) -> Option<String> {
    let root = workspace_root()?;
    let output = crate::git_exec::run_git(&root, args).ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

pub(crate) fn current_git_head_short() -> Option<String> {
    git_output(&["rev-parse", "--short", "HEAD"])
}

pub(crate) fn current_git_head() -> Option<String> {
    git_output(&["rev-parse", "HEAD"])
}

pub(crate) fn resolve_commit_to_full_oid(commit: &str) -> Option<String> {
    let commit = commit.trim();
    if commit.is_empty() {
        return None;
    }
    let root = workspace_root()?;
    git_output_in(
        &root,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )
    .ok()
}

pub(crate) fn commits_refer_to_same_oid(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }

    match (
        resolve_commit_to_full_oid(left),
        resolve_commit_to_full_oid(right),
    ) {
        (Some(left_oid), Some(right_oid)) => left_oid == right_oid,
        _ => {
            (left.len() >= 7 && right.starts_with(left))
                || (right.len() >= 7 && left.starts_with(right))
        }
    }
}

pub(crate) fn git_output_in(cwd: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = crate::git_exec::run_git(cwd, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!(
            "git {} failed in '{}': {}",
            args.join(" "),
            cwd.display(),
            detail
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn remove_git_worktree_force(repo_root: &std::path::Path, path: &std::path::Path) {
    let _ = crate::git_exec::run_git(
        repo_root,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    );
}

pub(crate) fn reviews_dir() -> Option<PathBuf> {
    brehon_root().map(|r| r.join("runtime").join("reviews"))
}

/// Suffix appended to a preflight worktree directory name to form the
/// corresponding lease file.  A lease file at
/// `{preflight_base}/{name}.lease` signals that the worktree named `{name}`
/// is actively owned by a live process.  The lease is created **before**
/// `git worktree add` so that no sweep can observe a markerless worktree.
const PREFLIGHT_LEASE_SUFFIX: &str = ".lease";

/// Creates a sidecar lease file in `preflight_base` for a preflight worktree
/// named `entry_name`.  The lease file contains the current process PID and
/// is written **before** the worktree directory is created so that a
/// concurrent sweep will always find the lease when it sees the worktree.
///
/// Returns the path to the lease file on success.
pub(crate) fn create_preflight_lease(
    preflight_base: &std::path::Path,
    entry_name: &str,
) -> Result<PathBuf, String> {
    let lease_path = preflight_base.join(format!("{entry_name}{PREFLIGHT_LEASE_SUFFIX}"));
    std::fs::write(&lease_path, std::process::id().to_string()).map_err(|err| {
        format!(
            "Failed to write preflight lease '{}': {err}",
            lease_path.display()
        )
    })?;
    Ok(lease_path)
}

/// Removes the sidecar lease file for a preflight worktree.  Called during
/// cleanup regardless of whether the worktree removal succeeded, so that
/// stale lease files do not accumulate.
pub(crate) fn remove_preflight_lease(preflight_base: &std::path::Path, entry_name: &str) {
    let lease_path = preflight_base.join(format!("{entry_name}{PREFLIGHT_LEASE_SUFFIX}"));
    let _ = std::fs::remove_file(&lease_path);
}

/// Checks whether a process with the given PID is still alive.
///
/// Uses `libc::kill(pid, 0)` (signal zero) which does not send a signal but
/// checks for process existence.  Returns `true` if the process exists (or
/// if the check cannot be performed, to be conservative), `false` if the
/// process is confirmed dead.
fn is_process_alive(pid: u32) -> bool {
    let pid_i32 = pid as i32;
    if pid as u64 != pid_i32 as u64 {
        return true;
    }
    let ret = unsafe { libc::kill(pid_i32, 0) };
    if ret == 0 {
        true
    } else {
        // Portable errno check: std::io::Error::last_os_error() works on
        // all Unix platforms (macOS uses __error, Linux uses __errno_location).
        // ESRCH means the process does not exist; any other error (e.g. EPERM)
        // means the process *does* exist, so conservatively treat it as alive.
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH as i32)
    }
}

/// Extracts the entry name (last path component) from a worktree path, if it
/// matches the expected preflight naming convention for `task_id`.
fn entry_name_for_task<'a>(path: &'a std::path::Path, task_id: &str) -> Option<&'a str> {
    let name = path.file_name()?.to_str()?;
    let prefix = format!("{task_id}-");
    if !name.starts_with(&prefix) {
        return None;
    }
    let suffix = name.rsplit('-').next().unwrap_or("");
    if suffix.len() < 12 || !suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(name)
}

/// Removes stale preflight worktrees belonging to `task_id` from the
/// `runtime/preflight/` directory.
///
/// If a previous process was killed between worktree creation and cleanup, the
/// temporary worktree directory, its git worktree registration, and its sidecar
/// lease file can be left behind.  This function sweeps the preflight directory
/// before a new worktree is created, reclaiming entries that:
///
/// - belong to the same `task_id` (names start with `{task_id}-`),
/// - end with a UUID-like suffix (at least 12 hex characters after the final
///   hyphen), matching the naming convention used by
///   `filter_commits_that_become_empty_when_replayed` and
///   `preview_commit_integration_conflicts`, and
/// - are confirmed abandoned: the sidecar lease file (`{name}.lease`) is either
///   missing or contains a PID that is no longer alive.
///
/// A worktree whose lease file contains a live PID is always skipped, even if
/// the process has been running for a long time.  This avoids the race that
/// directory-mtime-based heuristics cannot prevent: a cherry-pick running inside
/// a worktree modifies files within it but does not update the top-level
/// directory mtime, so a time-based threshold can incorrectly reclaim an active
/// worktree.
///
/// The lease file is created **before** `git worktree add`, so there is no
/// window where a worktree directory exists without a lease.  A concurrent
/// sweep that reads the directory between the lease creation and the worktree
/// creation will find the lease and skip the entry because the PID is alive,
/// even though the worktree directory does not yet exist.
///
/// Entries without a lease file are treated as abandoned — they were created by
/// an older version of the code that did not write lease files, and no live
/// process is tracking them.
///
/// Errors are logged but never propagated: a stale sweep is best-effort
/// hardening, not a gate on forward progress.
pub(crate) fn sweep_stale_preflight_worktrees(
    repo_root: &std::path::Path,
    preflight_base: &std::path::Path,
    task_id: &str,
) {
    let Ok(entries) = std::fs::read_dir(preflight_base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry_name_for_task(&path, task_id) else {
            continue;
        };

        let lease_path = preflight_base.join(format!("{name}{PREFLIGHT_LEASE_SUFFIX}"));
        if let Ok(lease_content) = std::fs::read_to_string(&lease_path) {
            if let Ok(pid) = lease_content.trim().parse::<u32>() {
                if is_process_alive(pid) {
                    tracing::debug!(
                        path = %path.display(),
                        pid = pid,
                        "Skipping preflight worktree with live owner process"
                    );
                    continue;
                }
                tracing::debug!(
                    path = %path.display(),
                    "Reclaiming preflight worktree with dead owner process"
                );
            } else {
                tracing::warn!(
                    path = %path.display(),
                    lease_content = %lease_content.trim(),
                    "Preflight worktree has unparseable lease PID; treating as abandoned"
                );
            }
        }

        remove_git_worktree_force(repo_root, &path);
        if path.exists() {
            if let Err(err) = std::fs::remove_dir_all(&path) {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "Failed to remove stale preflight worktree directory"
                );
            }
        }
        if lease_path.exists() {
            let _ = std::fs::remove_file(&lease_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn init_repo(root: &std::path::Path) {
        let output = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git init failed");
        let output = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git config email failed");
        let output = Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git config name failed");
        fs::write(root.join("README.md"), "seed\n").unwrap();
        let output = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git add failed");
        let output = Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git commit failed");
    }

    #[test]
    fn sweep_removes_abandoned_worktree_with_dead_pid() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let abandoned_dir = preflight_base.join("T-test-task-a1b2c3d4e5f6");
        fs::create_dir_all(&abandoned_dir).unwrap();
        fs::write(
            preflight_base.join("T-test-task-a1b2c3d4e5f6.lease"),
            "999999999",
        )
        .unwrap();

        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        assert!(
            !abandoned_dir.exists(),
            "abandoned worktree with dead PID should have been removed"
        );
        assert!(
            !preflight_base
                .join("T-test-task-a1b2c3d4e5f6.lease")
                .exists(),
            "lease file should have been cleaned up"
        );
    }

    #[test]
    fn sweep_preserves_worktree_with_live_pid() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let active_dir = preflight_base.join("T-test-task-a1b2c3d4e5f6");
        fs::create_dir_all(&active_dir).unwrap();
        fs::write(
            preflight_base.join("T-test-task-a1b2c3d4e5f6.lease"),
            std::process::id().to_string(),
        )
        .unwrap();

        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        assert!(
            active_dir.exists(),
            "worktree with live PID should not have been removed"
        );
        assert!(
            preflight_base
                .join("T-test-task-a1b2c3d4e5f6.lease")
                .exists(),
            "lease file should not have been removed for live worktree"
        );
    }

    #[test]
    fn sweep_removes_worktree_with_no_lease() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let no_lease_dir = preflight_base.join("T-test-task-a1b2c3d4e5f6");
        fs::create_dir_all(&no_lease_dir).unwrap();

        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        assert!(
            !no_lease_dir.exists(),
            "worktree without lease should have been removed (assumed abandoned)"
        );
    }

    #[test]
    fn sweep_does_not_touch_other_task_worktrees() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let other_dir = preflight_base.join("T-other-task-a1b2c3d4e5f6");
        fs::create_dir_all(&other_dir).unwrap();
        fs::write(
            preflight_base.join("T-other-task-a1b2c3d4e5f6.lease"),
            "999999999",
        )
        .unwrap();

        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        assert!(
            other_dir.exists(),
            "other task's worktree should not be removed"
        );
    }

    #[test]
    fn sweep_ignores_entries_without_uuid_suffix() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let no_uuid_dir = preflight_base.join("T-test-task-no-uuid");
        fs::create_dir_all(&no_uuid_dir).unwrap();

        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        assert!(
            no_uuid_dir.exists(),
            "entry without UUID suffix should not be removed"
        );
    }

    #[test]
    fn sweep_handles_resolve_prefix_entries() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let resolve_dir = preflight_base.join("T-test-task-resolve-a1b2c3d4e5f6");
        fs::create_dir_all(&resolve_dir).unwrap();
        fs::write(
            preflight_base.join("T-test-task-resolve-a1b2c3d4e5f6.lease"),
            "999999999",
        )
        .unwrap();

        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        assert!(
            !resolve_dir.exists(),
            "abandoned resolve worktree should have been removed"
        );
        assert!(
            !preflight_base
                .join("T-test-task-resolve-a1b2c3d4e5f6.lease")
                .exists(),
            "lease file for resolve worktree should have been cleaned up"
        );
    }

    #[test]
    fn sweep_removes_worktree_with_unparseable_pid() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let corrupt_dir = preflight_base.join("T-test-task-a1b2c3d4e5f6");
        fs::create_dir_all(&corrupt_dir).unwrap();
        fs::write(
            preflight_base.join("T-test-task-a1b2c3d4e5f6.lease"),
            "not-a-pid",
        )
        .unwrap();

        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        assert!(
            !corrupt_dir.exists(),
            "worktree with unparseable PID lease should have been removed (assumed abandoned)"
        );
    }

    #[test]
    fn create_preflight_lease_writes_current_pid() {
        let dir = tempfile::tempdir().unwrap();
        let preflight_base = dir.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();
        let lease_path =
            create_preflight_lease(&preflight_base, "T-test-task-a1b2c3d4e5f6").unwrap();
        assert!(lease_path.exists(), "lease file should exist");
        let content = std::fs::read_to_string(&lease_path).unwrap();
        let pid: u32 = content.trim().parse().unwrap();
        assert_eq!(pid, std::process::id(), "lease should contain current PID");
    }

    #[test]
    fn lease_created_before_worktree_prevents_reclaim() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let preflight_base = root.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        // Simulate the atomic lease-before-worktree pattern:
        // 1. Create lease FIRST (before git worktree add)
        let entry_name = "T-test-task-a1b2c3d4e5f6";
        create_preflight_lease(&preflight_base, entry_name).unwrap();

        // 2. Create the worktree directory (simulating git worktree add)
        let worktree_dir = preflight_base.join(entry_name);
        fs::create_dir_all(&worktree_dir).unwrap();

        // 3. Now run sweep — both lease and worktree exist with live PID
        sweep_stale_preflight_worktrees(root.path(), &preflight_base, "T-test-task");

        // Both the worktree directory and the lease should survive
        assert!(
            worktree_dir.exists(),
            "worktree with live lease should not have been removed"
        );
        assert!(
            preflight_base.join(format!("{entry_name}.lease")).exists(),
            "lease with live PID should not have been removed by sweep"
        );
    }

    #[test]
    fn remove_preflight_lease_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let preflight_base = dir.path().join("runtime").join("preflight");
        fs::create_dir_all(&preflight_base).unwrap();

        let entry_name = "T-test-task-a1b2c3d4e5f6";
        let lease_path = create_preflight_lease(&preflight_base, entry_name).unwrap();
        assert!(lease_path.exists());

        remove_preflight_lease(&preflight_base, entry_name);
        assert!(!lease_path.exists(), "lease should be removed");
    }
}
