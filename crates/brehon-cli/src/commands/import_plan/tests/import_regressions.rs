use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

use super::*;

struct CurrentDirGuard {
    original: PathBuf,
}

impl CurrentDirGuard {
    fn set(path: &Path) -> Self {
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { original }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

#[tokio::test]
async fn import_plan_with_relative_dot_path_uses_repo_identity_for_worktrees() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, normalized_plan_json()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    let _cwd = CurrentDirGuard::set(dir.path());
    execute(
        Path::new("."),
        Path::new("plan.json"),
        false,
        ExtractMode::Auto,
    )
    .await
    .unwrap();

    let config = brehon_config::load_config(Some(dir.path())).unwrap();
    let expected_worktree_root = crate::commands::run::effective_worktree_root(dir.path(), &config);
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let tasks = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            serde_json::from_str::<Value>(&fs::read_to_string(entry.path()).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();

    let worktree = tasks
        .iter()
        .filter_map(|task| task["integration_worktree"].as_str())
        .find(|path| path.contains("/initiative/") || path.contains("/epic/"))
        .expect("import should create integration worktrees");
    let worktree = Path::new(worktree);
    assert!(
        worktree.starts_with(&expected_worktree_root),
        "expected worktree '{}' to start with repo-scoped root '{}'",
        worktree.display(),
        expected_worktree_root.display()
    );
    assert!(
        !worktree.to_string_lossy().contains("/worktrees/unknown-"),
        "relative '.' project path must not produce unknown repo identity: {}",
        worktree.display()
    );
}
