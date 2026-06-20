//! Brehon worktree allowlisted cleanup.
//!
//! Brehon worktrees are disposable execution sandboxes, but arbitrary
//! untracked files can still contain copied scaffold or research. Mid-run
//! cleanup therefore only executes explicitly allowlisted actions.

use std::path::{Path, PathBuf};
use std::process::Command;

use brehon_types::{WorktreeCleanupActionConfig, WorktreeCleanupConfig};
use serde_json::Value;

use super::paths::{brehon_root_dir, brehon_worktrees_root, project_root};

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

        let value = serde_json::json!({
            "status": status,
            "phase": self.phase,
            "workspace": self.workspace,
            "removed": self.removed,
            "skipped": self.skipped,
            "errors": self.errors,
        });
        record_cleanup_audit(&value);
        value
    }
}

fn record_cleanup_audit(report: &Value) {
    let Some(root) = brehon_root_dir() else {
        return;
    };
    let runtime_dir = root.join("runtime");
    if std::fs::create_dir_all(&runtime_dir).is_err() {
        return;
    }
    let path = runtime_dir.join("worktree-cleanup.jsonl");
    let entry = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "report": report,
    });
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    use std::io::Write;
    let _ = writeln!(file, "{line}");
}

pub(crate) fn cleanup_current_worktree_allowlisted_artifacts(phase: &'static str) -> Value {
    let Some(workspace) = std::env::var("BREHON_WORKSPACE_ROOT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        let mut report = CleanupReport::new(phase);
        report.skip(
            "BREHON_WORKSPACE_ROOT",
            "No worker/reviewer workspace root is configured.",
        );
        return report.into_value();
    };

    cleanup_brehon_worktree_allowlisted_artifacts(phase, &workspace)
}

pub(super) fn cleanup_brehon_worktree_allowlisted_artifacts(
    phase: &'static str,
    workspace: &Path,
) -> Value {
    let mut report = CleanupReport::new(phase);
    report.workspace = Some(workspace.display().to_string());

    let canonical_workspace = match validate_worker_workspace(workspace) {
        Ok(path) => path,
        Err(err) => {
            report.skip(workspace.display().to_string(), err);
            return report.into_value();
        }
    };

    let repo = match git2::Repository::open(workspace) {
        Ok(repo) => repo,
        Err(err) => {
            report.skip(
                workspace.display().to_string(),
                format!("cannot open git worktree: {err}"),
            );
            return report.into_value();
        }
    };
    if repo.state() != git2::RepositoryState::Clean {
        report.skip(
            workspace.display().to_string(),
            format!("repository is in mid-operation state {:?}", repo.state()),
        );
        return report.into_value();
    }

    let actions = match cleanup_actions_for_phase(phase) {
        Ok(actions) => actions,
        Err(err) => {
            report.skip("worktree_cleanup", err);
            return report.into_value();
        }
    };
    if actions.is_empty() {
        report.skip("worktree_cleanup", "allowlisted cleanup is disabled");
        return report.into_value();
    }

    for action in actions {
        match action {
            WorktreeCleanupActionConfig::CargoClean { min_size_mb } => {
                cleanup_cargo_target(workspace, &canonical_workspace, min_size_mb, &mut report);
            }
        }
    }

    report.into_value()
}

fn cleanup_actions_for_phase(phase: &str) -> Result<Vec<WorktreeCleanupActionConfig>, String> {
    let config = configured_worktree_cleanup()?;
    if !config.enabled {
        return Ok(Vec::new());
    }
    let actions = match phase {
        "after_worker_handoff" => &config.on_worker_handoff,
        "after_review_submit" => &config.on_review_submit,
        "after_task_integrated" | "after_container_closed" => &config.on_terminal_cleanup,
        _ => &config.on_terminal_cleanup,
    };
    Ok(actions.clone())
}

fn configured_worktree_cleanup() -> Result<WorktreeCleanupConfig, String> {
    let Some(root) = project_root() else {
        return Ok(WorktreeCleanupConfig::default());
    };
    brehon_config::load_config(Some(&root))
        .map(|config| config.orchestration.worktree_cleanup)
        .map_err(|err| format!("Cannot load worktree cleanup config: {err}"))
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

    let worktrees_root = brehon_worktrees_root().ok_or_else(|| {
        "No BREHON_WORKTREE_ROOT or BREHON_ROOT available for build-artifact cleanup.".to_string()
    })?;
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

fn cleanup_cargo_target(
    workspace: &Path,
    canonical_workspace: &Path,
    min_size_mb: Option<u64>,
    report: &mut CleanupReport,
) {
    if !workspace.join("Cargo.toml").is_file() {
        report.skip("cargo_clean", "Cargo.toml is not present in this worktree");
        return;
    }

    let target = workspace.join("target");
    if !target.exists() {
        report.skip("target/", "no Cargo target directory is present");
        return;
    }

    let metadata = match std::fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(err) => {
            report.error(format!(
                "Cannot inspect Cargo target directory '{}': {err}",
                target.display()
            ));
            return;
        }
    };
    if metadata.file_type().is_symlink() {
        report.skip(
            "target/",
            "refusing cargo cleanup because target/ is a symlink",
        );
        return;
    }
    if !metadata.is_dir() {
        report.skip(
            "target/",
            "refusing cargo cleanup because target/ is not a directory",
        );
        return;
    }

    let canonical_target = match target.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            report.error(format!(
                "Cannot canonicalize Cargo target directory '{}': {err}",
                target.display()
            ));
            return;
        }
    };
    if !canonical_target.starts_with(canonical_workspace)
        || canonical_target.parent() != Some(canonical_workspace)
    {
        report.skip(
            "target/",
            format!(
                "refusing cargo cleanup because '{}' is not the worktree-local target directory",
                canonical_target.display()
            ),
        );
        return;
    }

    let target_size_bytes = match directory_size_bytes(&target) {
        Ok(size) => size,
        Err(err) => {
            report.error(format!(
                "Cannot measure Cargo target directory '{}': {err}",
                target.display()
            ));
            return;
        }
    };
    if let Some(min_size_mb) = min_size_mb {
        let min_size_bytes = min_size_mb.saturating_mul(1024 * 1024);
        if target_size_bytes < min_size_bytes {
            report.skip(
                "target/",
                format!(
                    "target/ is below cleanup threshold: {} MiB < {} MiB",
                    target_size_bytes / (1024 * 1024),
                    min_size_mb
                ),
            );
            return;
        }
    }

    match git_path_has_tracked_files(workspace, "target") {
        Ok(true) => {
            report.skip(
                "target/",
                "refusing cargo cleanup because target/ contains tracked files",
            );
            return;
        }
        Ok(false) => {}
        Err(err) => {
            report.error(err);
            return;
        }
    }

    match git_path_is_ignored(workspace, "target/") {
        Ok(true) => {}
        Ok(false) => {
            report.skip(
                "target/",
                "refusing cargo cleanup because target/ is not ignored by git",
            );
            return;
        }
        Err(err) => {
            report.error(err);
            return;
        }
    }

    let canonical_workspace_again = match workspace.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            report.error(format!(
                "Cannot canonicalize worker workspace '{}': {err}",
                workspace.display()
            ));
            return;
        }
    };
    if canonical_workspace_again != canonical_workspace {
        report.skip(
            workspace.display().to_string(),
            "workspace canonical path changed during cleanup",
        );
        return;
    }

    let output = match Command::new("cargo")
        .arg("clean")
        .arg("--target-dir")
        .arg(&target)
        .current_dir(workspace)
        .env_remove("CARGO_TARGET_DIR")
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            report.error(format!(
                "Failed to run cargo clean in '{}': {err}",
                workspace.display()
            ));
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        report.error(if stderr.is_empty() {
            format!(
                "cargo clean exited with {} in '{}'",
                output.status,
                workspace.display()
            )
        } else {
            stderr
        });
        return;
    }

    report.removed.push("target/".to_string());
}

fn directory_size_bytes(path: &Path) -> Result<u64, std::io::Error> {
    let mut total = 0_u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&current)? {
                stack.push(entry?.path());
            }
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn git_path_has_tracked_files(workspace: &Path, pathspec: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "--"])
        .arg(pathspec)
        .output()
        .map_err(|err| format!("Failed to inspect tracked files under {pathspec}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files failed for {pathspec}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn git_path_is_ignored(workspace: &Path, pathspec: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["check-ignore", "-q", "--"])
        .arg(pathspec)
        .output()
        .map_err(|err| format!("Failed to inspect git ignore status for {pathspec}: {err}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git check-ignore failed for {pathspec}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
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
            let keys = [
                "BREHON_ROOT",
                "BREHON_PROJECT_ROOT",
                "BREHON_WORKSPACE_ROOT",
            ];
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
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"cleanup-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        run_git(
            root,
            &["add", ".gitignore", "README.md", "Cargo.toml", "src/lib.rs"],
        );
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

    fn write_cleanup_config(root: &Path, worker_handoff_action: &str) {
        let brehon_dir = root.join(".brehon");
        std::fs::create_dir_all(&brehon_dir).unwrap();
        std::fs::write(
            brehon_dir.join("config.yaml"),
            format!(
                "orchestration:\n  worktree_cleanup:\n    enabled: true\n    on_worker_handoff:\n{worker_handoff_action}\n    on_review_submit: []\n    on_terminal_cleanup: []\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn removes_ignored_untracked_target_in_worker_worktree() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        write_cleanup_config(root.path(), "      - kind: cargo_clean");
        let worktree = add_worker_worktree(root.path(), "worker-1");
        let target_file = worktree.join("target/debug/app");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "artifact").unwrap();

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", worktree.to_str().unwrap()),
        ]);

        let report =
            cleanup_brehon_worktree_allowlisted_artifacts("after_worker_handoff", &worktree);

        assert_eq!(report["status"], "removed", "{report:#}");
        assert!(!worktree.join("target").exists());
    }

    #[test]
    fn skips_target_below_configured_size_threshold() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        write_cleanup_config(
            root.path(),
            "      - kind: cargo_clean\n        min_size_mb: 1",
        );
        let worktree = add_worker_worktree(root.path(), "worker-1");
        let target_file = worktree.join("target/debug/app");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "artifact").unwrap();

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", worktree.to_str().unwrap()),
        ]);

        let report =
            cleanup_brehon_worktree_allowlisted_artifacts("after_worker_handoff", &worktree);

        assert_eq!(report["status"], "skipped");
        assert!(worktree.join("target/debug/app").exists());
        assert!(report["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("below cleanup threshold"));
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

        let report =
            cleanup_brehon_worktree_allowlisted_artifacts("after_worker_handoff", root.path());

        assert_eq!(report["status"], "skipped");
        assert!(root.path().join("target").exists());
        assert!(report["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("primary project checkout"));
    }

    #[test]
    fn skips_target_when_it_contains_tracked_files() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let worktree = add_worker_worktree(root.path(), "worker-1");
        let tracked_target_file = worktree.join("target/keep.txt");
        std::fs::create_dir_all(tracked_target_file.parent().unwrap()).unwrap();
        std::fs::write(&tracked_target_file, "tracked").unwrap();
        run_git(&worktree, &["add", "-f", "target/keep.txt"]);
        run_git(&worktree, &["commit", "-m", "track target file"]);
        let untracked_target_file = worktree.join("target/debug/app");
        std::fs::create_dir_all(untracked_target_file.parent().unwrap()).unwrap();
        std::fs::write(&untracked_target_file, "artifact").unwrap();

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", worktree.to_str().unwrap()),
        ]);

        let report =
            cleanup_brehon_worktree_allowlisted_artifacts("after_worker_handoff", &worktree);

        assert_eq!(report["status"], "skipped");
        assert!(worktree.join("target/keep.txt").exists());
        assert!(worktree.join("target/debug/app").exists());
    }

    #[test]
    fn skips_unignored_target_directory() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        run_git(root.path(), &["init", "-b", "main"]);
        run_git(root.path(), &["config", "user.email", "test@example.com"]);
        run_git(root.path(), &["config", "user.name", "Test User"]);
        std::fs::write(root.path().join("README.md"), "seed\n").unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"cleanup-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        run_git(
            root.path(),
            &["add", "README.md", "Cargo.toml", "src/lib.rs"],
        );
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

        let report =
            cleanup_brehon_worktree_allowlisted_artifacts("after_worker_handoff", &worktree);

        assert_eq!(report["status"], "skipped");
        assert!(worktree.join("target/debug/app").exists());
    }

    #[test]
    fn cargo_allowlist_does_not_remove_scaffold_or_neighbor_junk() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        write_cleanup_config(root.path(), "      - kind: cargo_clean");
        let worktree = add_worker_worktree(root.path(), "worker-1");
        std::fs::create_dir_all(worktree.join(".agents")).unwrap();
        std::fs::create_dir_all(worktree.join(".claude")).unwrap();
        std::fs::write(worktree.join(".mcp.json"), "{}").unwrap();
        std::fs::write(worktree.join("opencode.json"), "{}").unwrap();
        std::fs::write(worktree.join(".agents/mcp_config.json"), "{}").unwrap();
        std::fs::write(worktree.join(".agents/cache.bin"), "cache").unwrap();
        std::fs::write(worktree.join(".claude/settings.local.json"), "{}").unwrap();
        std::fs::write(worktree.join(".claude/cache.bin"), "cache").unwrap();
        let target_file = worktree.join("target/debug/app");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "artifact").unwrap();

        let _env = EnvGuard::set(&[
            ("BREHON_ROOT", root.path().join(".brehon").to_str().unwrap()),
            ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", worktree.to_str().unwrap()),
        ]);

        let report =
            cleanup_brehon_worktree_allowlisted_artifacts("after_worker_handoff", &worktree);

        assert_eq!(report["status"], "removed", "{report:#}");
        assert!(worktree.join(".mcp.json").exists());
        assert!(worktree.join("opencode.json").exists());
        assert!(worktree.join(".agents/mcp_config.json").exists());
        assert!(worktree.join(".claude/settings.local.json").exists());
        assert!(worktree.join(".agents/cache.bin").exists());
        assert!(worktree.join(".claude/cache.bin").exists());
        assert!(!worktree.join("target").exists());
    }
}
