use std::path::{Path, PathBuf};

fn git_output_in(cwd: &Path, args: &[&str]) -> Result<String, String> {
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

fn cherry_pick_head_path(cwd: &Path) -> Option<PathBuf> {
    let path = git_output_in(cwd, &["rev-parse", "--git-path", "CHERRY_PICK_HEAD"]).ok()?;
    let path = PathBuf::from(path);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn error_indicates_empty_cherry_pick(cherry_pick_error: &str) -> bool {
    cherry_pick_error
        .to_ascii_lowercase()
        .contains("previous cherry-pick is now empty")
}

fn meaningful_status_entries(status: &str) -> Vec<&str> {
    status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.starts_with("?? "))
        .filter(|line| {
            let path = line.get(3..).unwrap_or("").trim();
            let path = path.rsplit(" -> ").next().unwrap_or(path);
            !path.starts_with(".brehon/")
        })
        .collect()
}

/// Returns true only when Git reports an actual empty cherry-pick state that is
/// safe to skip: the empty-cherry-pick diagnostic is present, CHERRY_PICK_HEAD
/// still exists, there are no unmerged files, and no tracked/non-.brehon changes
/// remain in the worktree. Untracked files are ignored so scratch artifacts do
/// not block a safe skip.
pub(crate) fn can_skip_failed_cherry_pick_as_empty(cwd: &Path, cherry_pick_error: &str) -> bool {
    if !error_indicates_empty_cherry_pick(cherry_pick_error) {
        return false;
    }

    let Some(cherry_pick_head) = cherry_pick_head_path(cwd) else {
        return false;
    };
    if !cherry_pick_head.exists() {
        return false;
    }

    let has_unmerged_files = git_output_in(cwd, &["diff", "--name-only", "--diff-filter=U"])
        .map(|stdout| !stdout.trim().is_empty())
        .unwrap_or(true);
    if has_unmerged_files {
        return false;
    }

    git_output_in(cwd, &["status", "--porcelain"])
        .map(|stdout| meaningful_status_entries(&stdout).is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::can_skip_failed_cherry_pick_as_empty;
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

    #[test]
    fn empty_cherry_pick_with_only_untracked_files_is_still_skippable() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        run_git(root.path(), &["checkout", "-b", "worker/task"]);
        std::fs::write(root.path().join("src.txt"), "shared implementation\n").unwrap();
        run_git(root.path(), &["add", "src.txt"]);
        run_git(root.path(), &["commit", "-m", "worker implementation"]);
        let reviewed_commit = run_git(root.path(), &["rev-parse", "HEAD"]);

        run_git(root.path(), &["checkout", "main"]);
        std::fs::write(root.path().join("src.txt"), "shared implementation\n").unwrap();
        run_git(root.path(), &["add", "src.txt"]);
        run_git(
            root.path(),
            &["commit", "-m", "main already has implementation"],
        );

        let scratch = root.path().join("scratch.txt");
        std::fs::write(&scratch, "untracked artifact\n").unwrap();
        let output = Command::new("git")
            .args(["cherry-pick", &reviewed_commit])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        assert!(can_skip_failed_cherry_pick_as_empty(root.path(), &stderr));
        run_git(root.path(), &["cherry-pick", "--abort"]);
    }

    #[test]
    fn merge_commit_failure_is_not_treated_as_empty() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        run_git(root.path(), &["checkout", "-b", "topic-a"]);
        std::fs::write(root.path().join("a.txt"), "a\n").unwrap();
        run_git(root.path(), &["add", "a.txt"]);
        run_git(root.path(), &["commit", "-m", "topic a"]);

        run_git(root.path(), &["checkout", "main"]);
        run_git(root.path(), &["checkout", "-b", "topic-b"]);
        std::fs::write(root.path().join("b.txt"), "b\n").unwrap();
        run_git(root.path(), &["add", "b.txt"]);
        run_git(root.path(), &["commit", "-m", "topic b"]);
        let topic_b_commit = run_git(root.path(), &["rev-parse", "HEAD"]);

        run_git(root.path(), &["checkout", "topic-a"]);
        run_git(
            root.path(),
            &["merge", "--no-ff", "topic-b", "-m", "merge topic b"],
        );
        let merge_commit = run_git(root.path(), &["rev-parse", "HEAD"]);
        assert_ne!(merge_commit, topic_b_commit);

        run_git(root.path(), &["checkout", "main"]);
        let output = Command::new("git")
            .args(["cherry-pick", &merge_commit])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        assert!(!can_skip_failed_cherry_pick_as_empty(root.path(), &stderr));
    }
}
