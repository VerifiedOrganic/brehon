//! Session-close repository hygiene.
//!
//! Brehon leaked worktree directories fast enough that `git worktree list`
//! stopped being a usable registry of what exists — most registrations in a
//! long-lived project were `prunable`, and every leaked one is a stale entry
//! that recovery, doctor and maintenance all have to reason about. See
//! `docs/CONTAINMENT_PLAN.md` T3.

use std::path::Path;

/// Run directories are `runs/<session>/<scope>/<agent>` — see
/// `scoped_worktree_path`. Two levels below the session directory, and
/// `<agent>` is a worktree root, so the sweep stops at the checkout boundary.
const RUN_DIR_DEPTH: usize = 2;

/// Drop worktree registrations whose directory is gone, then remove the run
/// directories they leave behind.
///
/// Best-effort: a session that has already finished its work must not fail
/// because a cleanup step did, so every error is logged and swallowed.
pub(crate) fn prune_session_worktrees(cwd: &Path, worktrees_root: &Path, session_name: &str) {
    match brehon_git::Git2Operations::open(cwd) {
        Ok(git) => match git.prune_stale_worktrees() {
            Ok(pruned) if !pruned.is_empty() => tracing::info!(
                count = pruned.len(),
                worktrees = ?pruned,
                "Pruned stale worktree registrations at session close"
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(
                error = %err,
                "Failed to prune stale worktree registrations at session close"
            ),
        },
        Err(err) => tracing::warn!(
            path = %cwd.display(),
            error = %err,
            "Could not open git repository to prune stale worktree registrations"
        ),
    }

    // Only this session's own run directory. A concurrent session creates its
    // `runs/<session>/<scope>` parent *before* asking libgit2 to add the
    // worktree into it, and libgit2 does not recreate a missing parent — so
    // sweeping the shared `runs/` root would abort a peer run with ENOENT.
    let session_runs = worktrees_root.join("runs").join(session_name);
    let mut swept = sweep_empty_dirs(&session_runs, RUN_DIR_DEPTH);
    if std::fs::remove_dir(&session_runs).is_ok() {
        swept += 1;
    }
    if swept > 0 {
        tracing::info!(
            count = swept,
            "Swept empty run directories at session close"
        );
    }

    // Reconcile Brehon's own inventory against the filesystem. A killed run
    // leaves entries whose directory is gone; under ADR-0010 this inventory
    // replaces `git worktree list` as the answer to what exists, so it has to
    // stay honest without asking git anything.
    let dropped = super::agent_inventory::reconcile(&cwd.join(".brehon"));
    if !dropped.is_empty() {
        tracing::info!(
            count = dropped.len(),
            agents = ?dropped.iter().map(|e| e.agent_id.as_str()).collect::<Vec<_>>(),
            "Dropped agent inventory entries whose directory is gone"
        );
    }
}

/// Depth-first removal of empty directories at most `depth` levels beneath
/// `root`; `root` itself is left to the caller. Returns how many were removed.
///
/// The depth bound is load-bearing, not a micro-optimisation. `<agent>` is a
/// worktree root, so an unbounded walk would descend into live checkouts —
/// which are kept whenever `auto_cleanup_worktrees` is false — and delete
/// empty directories git neither tracks nor can restore. `remove_dir` refuses
/// a non-empty directory, so a live worktree root is itself never removed, and
/// `file_type()` does not follow symlinks.
fn sweep_empty_dirs(root: &Path, depth: usize) -> usize {
    if depth == 0 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };

    let mut swept = 0;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        swept += sweep_empty_dirs(&path, depth - 1);
        if std::fs::remove_dir(&path).is_ok() {
            swept += 1;
        }
    }
    swept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(path: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_git_repo(path: &Path) {
        run_git(path, &["init", "-b", "main"]);
        run_git(path, &["config", "user.email", "brehon@example.invalid"]);
        run_git(path, &["config", "user.name", "Brehon Test"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        run_git(path, &["add", "README.md"]);
        run_git(path, &["commit", "-m", "seed"]);
    }

    /// Add a worktree at `<worktrees_root>/runs/<session>/worker/w1`, the
    /// layout `scoped_worktree_path` produces.
    fn add_run_worktree(repo: &Path, worktrees_root: &Path, session: &str) -> std::path::PathBuf {
        let worktree_path = worktrees_root.join(format!("runs/{session}/worker/w1"));
        std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
        run_git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                &format!("brehon/runs/{session}/w1"),
                worktree_path.to_string_lossy().as_ref(),
            ],
        );
        worktree_path
    }

    /// T3 acceptance criterion: after a session close,
    /// `git worktree list --porcelain | grep -c prunable` returns 0, and the
    /// run directories the dead worktrees occupied are gone.
    #[test]
    fn prune_session_worktrees_clears_prunable_registrations_and_empty_run_dirs() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let worktrees_root = temp.path().join("worktrees-root");
        let worktree_path = add_run_worktree(temp.path(), &worktrees_root, "session-a");

        // Simulate the leak this ticket exists for: the directory disappears
        // while the registration survives.
        std::fs::remove_dir_all(&worktree_path).unwrap();
        assert!(
            run_git(temp.path(), &["worktree", "list", "--porcelain"]).contains("prunable"),
            "fixture should leave a prunable registration behind"
        );

        prune_session_worktrees(temp.path(), &worktrees_root, "session-a");

        assert!(
            !run_git(temp.path(), &["worktree", "list", "--porcelain"]).contains("prunable"),
            "session close must leave no prunable worktree registrations"
        );
        assert!(
            !worktrees_root.join("runs/session-a").exists(),
            "empty run directories must be swept at session close"
        );
        assert!(
            worktrees_root.join("runs").exists(),
            "the runs root itself is kept"
        );
    }

    #[test]
    fn prune_session_worktrees_keeps_live_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let worktrees_root = temp.path().join("worktrees-root");
        let worktree_path = add_run_worktree(temp.path(), &worktrees_root, "session-live");

        // A retained worktree — `auto_cleanup_worktrees = false` — must come
        // through untouched, including empty directories inside its checkout,
        // which git neither tracks nor can restore.
        let scratch = worktree_path.join("build/scratch");
        std::fs::create_dir_all(&scratch).unwrap();

        prune_session_worktrees(temp.path(), &worktrees_root, "session-live");

        assert!(
            worktree_path.exists(),
            "a live worktree must survive session-close hygiene"
        );
        assert!(
            scratch.exists(),
            "the sweep must not descend into a live checkout"
        );
        assert!(
            run_git(temp.path(), &["worktree", "list", "--porcelain"])
                .contains(worktree_path.to_string_lossy().as_ref()),
            "a live worktree must stay registered"
        );
    }

    /// A peer session creates `runs/<session>/<scope>` before libgit2 adds the
    /// worktree into it, and libgit2 will not recreate a missing parent — so a
    /// closing session must not touch anyone else's run directory.
    #[test]
    fn prune_session_worktrees_leaves_other_sessions_alone() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let worktrees_root = temp.path().join("worktrees-root");
        let peer_parent = worktrees_root.join("runs/session-peer/worker");
        std::fs::create_dir_all(&peer_parent).unwrap();

        prune_session_worktrees(temp.path(), &worktrees_root, "session-mine");

        assert!(
            peer_parent.exists(),
            "a peer session's pre-created worktree parent must survive"
        );
    }

    #[test]
    fn sweep_empty_dirs_leaves_non_empty_trees_alone() {
        let temp = tempfile::tempdir().unwrap();
        let runs = temp.path().join("runs");
        std::fs::create_dir_all(runs.join("live/worker/w1")).unwrap();
        std::fs::write(runs.join("live/worker/w1/file.txt"), "in use\n").unwrap();
        std::fs::create_dir_all(runs.join("dead/worker/w1")).unwrap();

        let swept = sweep_empty_dirs(&runs, RUN_DIR_DEPTH + 1);

        assert_eq!(
            swept, 3,
            "dead/, dead/worker/ and dead/worker/w1/ are empty"
        );
        assert!(runs.join("live/worker/w1/file.txt").exists());
        assert!(!runs.join("dead").exists());
    }

    /// The depth bound is what stops the sweep at the checkout boundary.
    #[test]
    fn sweep_empty_dirs_stops_at_the_depth_bound() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("session");
        std::fs::create_dir_all(session.join("worker/w1/inside/checkout")).unwrap();

        let swept = sweep_empty_dirs(&session, RUN_DIR_DEPTH);

        assert_eq!(swept, 0, "w1/ is non-empty and nothing below it is visited");
        assert!(session.join("worker/w1/inside/checkout").exists());
    }
}
