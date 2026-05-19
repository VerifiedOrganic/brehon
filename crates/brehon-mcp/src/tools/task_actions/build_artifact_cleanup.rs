//! Worker-local build artifact cleanup.
//!
//! This is intentionally narrow: remove only ignored, untracked top-level
//! build directories from the current worker worktree. It must never become a
//! general `git clean` replacement.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::paths::brehon_root_dir;

const CLEANUP_DIRS: &[&str] = &["target"];

#[derive(Debug, Default)]
struct CleanupReport {
    phase: &'static str,
    workspace: Option<String>,
    removed: Vec<String>,
    skipped: Vec<Value>,
    errors: Vec<String>,
}

impl CleanupReport {
    fn new(phase: &'static str) -> Self {
        Self {
            phase,
            ..Self::default()
        }
    }

    fn skip(&mut self, path: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push(serde_json::json!({
            "path": path.into(),
            "reason": reason.into(),
        }));
    }

    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    fn into_value(self) -> Value {
        let status = if !self.errors.is_empty() {
            "error"
        } else if !self.removed.is_empty() {
            "removed"
        } else if !self.skipped.is_empty() {
            "skipped"
        } else {
            "noop"
        };

        serde_json::json!({
            "status": status,
            "phase": self.phase,
            "workspace": self.workspace,
            "removed": self.removed,
            "skipped": self.skipped,
            "errors": self.errors,
        })
    }
}

pub(super) fn cleanup_current_worker_build_artifacts(phase: &'static str) -> Value {
    let mut report = CleanupReport::new(phase);

    let workspace = match worker_workspace_from_env() {
        Ok(workspace) => workspace,
        Err(err) => {
            report.skip("BREHON_WORKSPACE_ROOT", err);
            return report.into_value();
        }
    };
    report.workspace = Some(workspace.display().to_string());

    let canonical_workspace = match validate_worker_workspace(&workspace) {
        Ok(path) => path,
        Err(err) => {
            report.skip(workspace.display().to_string(), err);
            return report.into_value();
        }
    };

    if let Ok(repo) = git2::Repository::open(&workspace) {
        if repo.state() != git2::RepositoryState::Clean {
            report.skip(
                workspace.display().to_string(),
                format!("repository is in mid-operation state {:?}", repo.state()),
            );
            return report.into_value();
        }
    }

    for dir in CLEANUP_DIRS {
        cleanup_dir(&workspace, &canonical_workspace, dir, &mut report);
    }

    report.into_value()
}

fn worker_workspace_from_env() -> Result<PathBuf, String> {
    std::env::var("BREHON_WORKSPACE_ROOT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "BREHON_WORKSPACE_ROOT is not set; refusing build-artifact cleanup.".to_string()
        })
}

fn validate_worker_workspace(workspace: &Path) -> Result<PathBuf, String> {
    let canonical_workspace = workspace.canonicalize().map_err(|err| {
        format!(
            "Cannot canonicalize worker workspace '{}': {err}",
            workspace.display()
        )
    })?;

    if !workspace.join(".git").exists() {
        return Err(format!(
            "Refusing build-artifact cleanup in '{}': not a git worktree.",
            workspace.display()
        ));
    }

    if let Ok(project_root) = std::env::var("BREHON_PROJECT_ROOT") {
        let project_root = project_root.trim();
        if !project_root.is_empty() {
            let project_root = PathBuf::from(project_root);
            if project_root
                .canonicalize()
                .ok()
                .is_some_and(|canonical_project| canonical_project == canonical_workspace)
            {
                return Err(format!(
                    "Refusing build-artifact cleanup in '{}': this is the primary project checkout.",
                    workspace.display()
                ));
            }
        }
    }

    let brehon_root = brehon_root_dir()
        .ok_or_else(|| "No BREHON_ROOT available for build-artifact cleanup.".to_string())?;
    let worktrees_root = brehon_root.join("worktrees");
    let canonical_worktrees = worktrees_root.canonicalize().map_err(|err| {
        format!(
            "Cannot canonicalize Brehon worktrees root '{}': {err}",
            worktrees_root.display()
        )
    })?;

    if !canonical_workspace.starts_with(&canonical_worktrees) {
        return Err(format!(
            "Refusing build-artifact cleanup in '{}': workspace is outside Brehon-owned worktrees under '{}'.",
            workspace.display(),
            canonical_worktrees.display()
        ));
    }

    Ok(canonical_workspace)
}

fn cleanup_dir(
    workspace: &Path,
    canonical_workspace: &Path,
    relative_dir: &str,
    report: &mut CleanupReport,
) {
    let path = workspace.join(relative_dir);
    let display_path = path.display().to_string();
    if !path.exists() {
        return;
    }

    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) => {
            report.error(format!("Cannot inspect '{}': {err}", path.display()));
            return;
        }
    };

    if metadata.file_type().is_symlink() {
        report.skip(display_path, "path is a symlink");
        return;
    }
    if !metadata.is_dir() {
        report.skip(display_path, "path is not a directory");
        return;
    }

    let canonical_path = match path.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            report.error(format!("Cannot canonicalize '{}': {err}", path.display()));
            return;
        }
    };
    if !canonical_path.starts_with(canonical_workspace) {
        report.skip(display_path, "canonical path escapes the worker workspace");
        return;
    }

    match path_has_tracked_files(workspace, relative_dir) {
        Ok(true) => {
            report.skip(display_path, "directory contains tracked files");
            return;
        }
        Ok(false) => {}
        Err(err) => {
            report.error(err);
            return;
        }
    }

    match path_is_ignored(workspace, relative_dir) {
        Ok(true) => {}
        Ok(false) => {
            report.skip(display_path, "directory is not ignored by git");
            return;
        }
        Err(err) => {
            report.error(err);
            return;
        }
    }

    match std::fs::remove_dir_all(&path) {
        Ok(()) => {
            report.removed.push(display_path);
        }
        Err(err) => {
            report.error(format!("Failed to remove '{}': {err}", path.display()));
        }
    }
}

fn path_has_tracked_files(workspace: &Path, relative_dir: &str) -> Result<bool, String> {
    let output = crate::git_exec::run_git(workspace, &["ls-files", "--", relative_dir])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git ls-files -- {relative_dir} exited with {}",
                output.status
            )
        } else {
            stderr
        });
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn path_is_ignored(workspace: &Path, relative_dir: &str) -> Result<bool, String> {
    let output = crate::git_exec::run_git(workspace, &["check-ignore", "-q", "--", relative_dir])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                format!(
                    "git check-ignore -q -- {relative_dir} exited with {}",
                    output.status
                )
            } else {
                stderr
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::TEST_ENV_LOCK;
    use std::ffi::OsString;
    use std::process::Command;

    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let keys = ["BREHON_ROOT", "BREHON_PROJECT_ROOT", "BREHON_WORKSPACE_ROOT"];
            let mut saved = Vec::new();
            for key in keys {
                saved.push((key, std::env::var_os(key)));
                if let Some((_, value)) = vars.iter().find(|(candidate, _)| candidate == &key) {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
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

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(root: &Path) {
        run_git(root, &["init", "-b", "main"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test User"]);
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("README.md"), "seed\n").unwrap();
        run_git(root, &["add", ".gitignore", "README.md"]);
        run_git(root, &["commit", "-m", "seed"]);
    }

    fn add_worker_worktree(root: &Path, worker: &str) -> PathBuf {
        let worktree = root.join(".brehon/worktrees").join(worker);
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            root,
            &[
                "worktree",
                "add",
                "-b",
                &format!("worker/{worker}"),
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        worktree
    }

    #[test]
    fn removes_ignored_untracked_target_in_worker_worktree() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let worktree = add_worker_worktree(root.path(), "worker-1");
        let target_file = worktree.join("target/debug/app");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "artifact").unwrap();

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", worktree.to_str().unwrap()),
        ]);

        let report = cleanup_current_worker_build_artifacts("test");

        assert_eq!(report["status"], "removed");
        assert!(!worktree.join("target").exists());
    }

    #[test]
    fn refuses_primary_project_checkout() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let target_file = root.path().join("target/debug/app");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "artifact").unwrap();

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", root.path().to_str().unwrap()),
        ]);

        let report = cleanup_current_worker_build_artifacts("test");

        assert_eq!(report["status"], "skipped");
        assert!(root.path().join("target").exists());
        assert!(report["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("primary project checkout"));
    }

    #[test]
    fn skips_target_with_tracked_files() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let worktree = add_worker_worktree(root.path(), "worker-1");
        let tracked_target_file = worktree.join("target/keep.txt");
        std::fs::create_dir_all(tracked_target_file.parent().unwrap()).unwrap();
        std::fs::write(&tracked_target_file, "tracked").unwrap();
        run_git(&worktree, &["add", "-f", "target/keep.txt"]);
        run_git(&worktree, &["commit", "-m", "track target file"]);

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", worktree.to_str().unwrap()),
        ]);

        let report = cleanup_current_worker_build_artifacts("test");

        assert_eq!(report["status"], "skipped");
        assert!(worktree.join("target/keep.txt").exists());
        assert_eq!(
            report["skipped"][0]["reason"],
            "directory contains tracked files"
        );
    }

    #[test]
    fn skips_unignored_target_directory() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        run_git(root.path(), &["init", "-b", "main"]);
        run_git(root.path(), &["config", "user.email", "test@example.com"]);
        run_git(root.path(), &["config", "user.name", "Test User"]);
        std::fs::write(root.path().join("README.md"), "seed\n").unwrap();
        run_git(root.path(), &["add", "README.md"]);
        run_git(root.path(), &["commit", "-m", "seed"]);
        let worktree = add_worker_worktree(root.path(), "worker-1");
        let target_file = worktree.join("target/debug/app");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "artifact").unwrap();

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", worktree.to_str().unwrap()),
        ]);

        let report = cleanup_current_worker_build_artifacts("test");

        assert_eq!(report["status"], "skipped");
        assert!(worktree.join("target/debug/app").exists());
        assert_eq!(
            report["skipped"][0]["reason"],
            "directory is not ignored by git"
        );
    }
}
