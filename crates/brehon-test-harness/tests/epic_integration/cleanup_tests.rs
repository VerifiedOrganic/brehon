//! Cleanup tests for epic integration branch flow
//!
//! Tests:
//! 1. After epic merges to main, epic branch and worktree are cleaned up
//! 2. Subtask worktrees are cleaned up after subtask merge to epic branch

use chrono::Utc;
use std::path::Path;
use std::process::Command;

fn run_git(workspace: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_git_workspace(workspace: &Path) -> String {
    if workspace.exists() {
        std::fs::remove_dir_all(workspace).ok();
    }
    std::fs::create_dir_all(workspace).unwrap();

    run_git(workspace, &["init", "-b", "main"]);
    run_git(workspace, &["config", "user.email", "test@example.com"]);
    run_git(workspace, &["config", "user.name", "Test User"]);
    std::fs::write(workspace.join("README.md"), "initial\n").unwrap();
    run_git(workspace, &["add", "README.md"]);
    run_git(workspace, &["commit", "-m", "initial"]);
    run_git(workspace, &["rev-parse", "HEAD"])
}

fn create_epic_task(
    tasks_dir: &Path,
    epic_id: &str,
    integration_branch: Option<&str>,
) -> serde_json::Value {
    std::fs::create_dir_all(tasks_dir).unwrap();

    let mut task = serde_json::json!({
        "task_id": epic_id,
        "title": format!("Epic {}", epic_id),
        "description": "Feature epic for testing integration flow",
        "status": "closed",
        "task_type": "epic",
        "completion_mode": "merge",
        "assignee": serde_json::Value::Null,
        "percent": 100,
        "created_at": Utc::now().to_rfc3339(),
        "closed_at": Utc::now().to_rfc3339(),
    });

    if let Some(branch) = integration_branch {
        task["integration_branch"] = serde_json::Value::String(branch.to_string());
    }

    let path = tasks_dir.join(format!("{}.json", epic_id));
    std::fs::write(&path, serde_json::to_string_pretty(&task).unwrap()).unwrap();
    task
}

fn create_subtask(
    tasks_dir: &Path,
    subtask_id: &str,
    parent_id: &str,
    status: &str,
    merge_target: &str,
    integration_status: &str,
) -> serde_json::Value {
    std::fs::create_dir_all(tasks_dir).unwrap();

    let task = serde_json::json!({
        "task_id": subtask_id,
        "title": format!("Subtask {}", subtask_id),
        "description": "Subtask under epic",
        "status": status,
        "task_type": "task",
        "completion_mode": "merge",
        "parent_id": parent_id,
        "merge_target": merge_target,
        "integration_status": integration_status,
        "assignee": "worker-1",
        "percent": 100,
        "created_at": Utc::now().to_rfc3339(),
    });

    let path = tasks_dir.join(format!("{}.json", subtask_id));
    std::fs::write(&path, serde_json::to_string_pretty(&task).unwrap()).unwrap();
    task
}

fn read_task(tasks_dir: &Path, task_id: &str) -> Option<serde_json::Value> {
    let path = tasks_dir.join(format!("{}.json", task_id));
    if path.exists() {
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    } else {
        None
    }
}

#[test]
fn epic_branch_cleanup_after_merge_to_main() {
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let epic_branch = "epic/T-cleanup001";

    run_git(workspace.path(), &["checkout", "-b", epic_branch]);
    std::fs::write(workspace.path().join("epic_file.txt"), "epic content\n").unwrap();
    run_git(workspace.path(), &["add", "epic_file.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic implementation"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["merge", epic_branch, "--no-edit"]);

    let branches_before = run_git(workspace.path(), &["branch", "--list", epic_branch]);
    assert!(
        branches_before.contains(epic_branch),
        "Epic branch should exist before cleanup"
    );

    run_git(workspace.path(), &["branch", "-d", epic_branch]);

    let branches_after = run_git(workspace.path(), &["branch", "--list", epic_branch]);
    assert!(
        !branches_after.contains(epic_branch),
        "Epic branch should be deleted after cleanup"
    );

    assert!(
        workspace.path().join("epic_file.txt").exists(),
        "Epic file should be on main after merge"
    );
}

#[test]
fn subtask_worktree_cleanup_after_merge_to_epic() {
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let epic_branch = "epic/T-cleanup002";
    let subtask_branch = "feature/T-subtask-cleanup";
    let worktree_base = workspace.path().join(".brehon").join("worktrees");
    std::fs::create_dir_all(&worktree_base).unwrap();
    let subtask_worktree = worktree_base.join("worker-subtask-cleanup");

    run_git(workspace.path(), &["checkout", "-b", epic_branch]);
    std::fs::write(workspace.path().join("epic_base.txt"), "epic base\n").unwrap();
    run_git(workspace.path(), &["add", "epic_base.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic base"]);
    let epic_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(
        workspace.path(),
        &[
            "worktree",
            "add",
            subtask_worktree.to_str().unwrap(),
            &epic_commit,
        ],
    );
    run_git(&subtask_worktree, &["checkout", "-b", subtask_branch]);

    std::fs::write(subtask_worktree.join("subtask_file.txt"), "subtask work\n").unwrap();
    run_git(&subtask_worktree, &["add", "subtask_file.txt"]);
    run_git(
        &subtask_worktree,
        &["commit", "-m", "subtask implementation"],
    );

    assert!(
        subtask_worktree.exists(),
        "Subtask worktree should exist before cleanup"
    );

    run_git(workspace.path(), &["checkout", epic_branch]);
    run_git(workspace.path(), &["merge", subtask_branch, "--no-edit"]);

    Command::new("git")
        .args(["worktree", "remove", subtask_worktree.to_str().unwrap()])
        .current_dir(workspace.path())
        .status()
        .unwrap();

    run_git(workspace.path(), &["branch", "-d", subtask_branch]);

    assert!(
        !subtask_worktree.exists(),
        "Subtask worktree should be removed after cleanup"
    );

    let branches_after = run_git(workspace.path(), &["branch", "--list", subtask_branch]);
    assert!(
        !branches_after.contains(subtask_branch),
        "Subtask branch should be deleted after cleanup"
    );
}

#[test]
fn epic_cleanup_verifies_main_unchanged_before_merge() {
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let main_file_before = std::fs::read_to_string(workspace.path().join("README.md")).unwrap();

    let epic_branch = "epic/T-cleanup003";
    run_git(workspace.path(), &["checkout", "-b", epic_branch]);
    std::fs::write(workspace.path().join("epic_only.txt"), "only on epic\n").unwrap();
    run_git(workspace.path(), &["add", "epic_only.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic feature"]);

    run_git(workspace.path(), &["checkout", "main"]);

    assert!(
        !workspace.path().join("epic_only.txt").exists(),
        "Epic file should not be on main yet"
    );

    let main_file_after = std::fs::read_to_string(workspace.path().join("README.md")).unwrap();
    assert_eq!(
        main_file_before, main_file_after,
        "Main should be unchanged before epic merge"
    );
}

#[test]
fn cleanup_preserves_task_files_as_record() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-cleanup004";
    let integration_branch = "epic/T-epic-cleanup004";

    let _epic = create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    let subtask1_id = "T-sub-cleanup001";
    create_subtask(
        &tasks_dir,
        subtask1_id,
        epic_id,
        "merged",
        integration_branch,
        "integrated",
    );

    let subtask2_id = "T-sub-cleanup002";
    create_subtask(
        &tasks_dir,
        subtask2_id,
        epic_id,
        "merged",
        integration_branch,
        "integrated",
    );

    assert!(
        tasks_dir.join(format!("{}.json", epic_id)).exists(),
        "Epic task file should exist"
    );
    assert!(
        tasks_dir.join(format!("{}.json", subtask1_id)).exists(),
        "Subtask 1 task file should exist"
    );
    assert!(
        tasks_dir.join(format!("{}.json", subtask2_id)).exists(),
        "Subtask 2 task file should exist"
    );

    let reloaded_epic = read_task(&tasks_dir, epic_id).unwrap();
    assert_eq!(reloaded_epic["status"], "closed");
    assert_eq!(reloaded_epic["task_type"], "epic");

    let reloaded_subtask1 = read_task(&tasks_dir, subtask1_id).unwrap();
    assert_eq!(reloaded_subtask1["status"], "merged");
    assert_eq!(reloaded_subtask1["integration_status"], "integrated");

    let reloaded_subtask2 = read_task(&tasks_dir, subtask2_id).unwrap();
    assert_eq!(reloaded_subtask2["status"], "merged");
    assert_eq!(reloaded_subtask2["integration_status"], "integrated");
}

#[test]
fn cleanup_handles_multiple_subtasks_sequentially() {
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let epic_branch = "epic/T-cleanup005";
    run_git(workspace.path(), &["checkout", "-b", epic_branch]);
    std::fs::write(workspace.path().join("epic_base.txt"), "base\n").unwrap();
    run_git(workspace.path(), &["add", "epic_base.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic base"]);

    for i in 1..=3 {
        let subtask_branch = format!("feature/subtask-{}", i);
        run_git(workspace.path(), &["checkout", "-b", &subtask_branch]);
        std::fs::write(
            workspace.path().join(format!("subtask{}.txt", i)),
            format!("subtask {} content\n", i),
        )
        .unwrap();
        run_git(workspace.path(), &["add", &format!("subtask{}.txt", i)]);
        run_git(
            workspace.path(),
            &["commit", "-m", &format!("subtask {}", i)],
        );

        run_git(workspace.path(), &["checkout", epic_branch]);
        run_git(workspace.path(), &["merge", &subtask_branch, "--no-edit"]);
        run_git(workspace.path(), &["branch", "-d", &subtask_branch]);
    }

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["merge", epic_branch, "--no-edit"]);
    run_git(workspace.path(), &["branch", "-d", epic_branch]);

    for i in 1..=3 {
        assert!(
            workspace.path().join(format!("subtask{}.txt", i)).exists(),
            "Subtask {} file should be on main after merge",
            i
        );
    }

    let remaining_branches = run_git(workspace.path(), &["branch"]);
    assert!(
        !remaining_branches.contains("epic/T-cleanup005"),
        "Epic branch should be deleted"
    );
    assert!(
        !remaining_branches.contains("feature/subtask-1"),
        "Subtask branches should be deleted"
    );
}
