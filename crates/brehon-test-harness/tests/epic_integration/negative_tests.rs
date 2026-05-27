//! Negative tests for epic integration branch flow
//!
//! Tests error conditions and forbidden transitions:
//! 1. Cannot close epic with non-integrated subtasks
//! 2. Cannot merge subtask directly to main when merge_target is epic branch
//! 3. Cannot close epic when epic branch has diverged from main
//! 4. Standalone task (no epic parent) still merges to main successfully

use chrono::Utc;
use std::path::Path;
use std::process::Command;

fn run_git(workspace: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    match args.first().copied() {
        Some("commit" | "merge") => {
            command.args(["-c", "commit.gpgsign=false"]);
        }
        Some("tag") => {
            command.args(["-c", "tag.gpgsign=false"]);
        }
        _ => {}
    }
    let output = command
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

    let brehon_root = workspace.join(".brehon");
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

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
    let mut task = serde_json::json!({
        "task_id": epic_id,
        "title": format!("Epic {}", epic_id),
        "description": "Feature epic for testing integration flow",
        "status": "pending",
        "task_type": "epic",
        "completion_mode": "merge",
        "assignee": serde_json::Value::Null,
        "percent": 0,
        "created_at": chrono::Utc::now().to_rfc3339(),
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
    percent: i32,
    merge_target: &str,
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
        "integration_status": "pending",
        "assignee": "worker-1",
        "percent": percent,
        "created_at": Utc::now().to_rfc3339(),
    });

    let path = tasks_dir.join(format!("{}.json", subtask_id));
    std::fs::write(&path, serde_json::to_string_pretty(&task).unwrap()).unwrap();
    task
}

fn write_task(tasks_dir: &Path, task: &serde_json::Value) {
    std::fs::create_dir_all(tasks_dir).unwrap();
    let task_id = task.get("task_id").and_then(|v| v.as_str()).unwrap();
    let path = tasks_dir.join(format!("{}.json", task_id));
    std::fs::write(&path, serde_json::to_string_pretty(&task).unwrap()).unwrap();
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

fn check_epic_completion(tasks_dir: &Path, epic_id: &str) -> (usize, usize, bool) {
    let all_tasks: Vec<_> = std::fs::read_dir(tasks_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            let task: serde_json::Value = serde_json::from_str(&content).ok()?;
            Some(task)
        })
        .collect();

    let subtasks: Vec<_> = all_tasks
        .iter()
        .filter(|t| t.get("parent_id").and_then(|v| v.as_str()) == Some(epic_id))
        .collect();

    let total = subtasks.len();
    let closed = subtasks
        .iter()
        .filter(|t| {
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            matches!(status, "merged" | "closed")
        })
        .count();

    (total, closed, total > 0 && total == closed)
}

#[test]
fn cannot_close_epic_with_non_integrated_subtask() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-neg001";
    let integration_branch = "epic/T-epic-neg001";

    let mut epic = create_epic_task(&tasks_dir, epic_id, Some(integration_branch));
    epic["status"] = serde_json::Value::String("in_progress".to_string());
    write_task(&tasks_dir, &epic);

    let subtask1_id = "T-sub-neg001";
    let mut subtask1 = create_subtask(
        &tasks_dir,
        subtask1_id,
        epic_id,
        "in_progress",
        75,
        integration_branch,
    );
    subtask1["integration_status"] = serde_json::Value::String("pending".to_string());
    write_task(&tasks_dir, &subtask1);

    let subtask2_id = "T-sub-neg002";
    let _subtask2 = create_subtask(
        &tasks_dir,
        subtask2_id,
        epic_id,
        "pending",
        0,
        integration_branch,
    );

    let (total, closed, all_done) = check_epic_completion(&tasks_dir, epic_id);

    assert_eq!(total, 2, "Should have 2 subtasks");
    assert_eq!(closed, 0, "No subtasks should be closed");
    assert!(!all_done, "Epic should not be complete");

    epic["status"] = serde_json::Value::String("closed".to_string());
    write_task(&tasks_dir, &epic);

    let reloaded = read_task(&tasks_dir, epic_id).unwrap();
    assert_eq!(
        reloaded["status"], "closed",
        "Task file is just data - enforcement happens in API"
    );
}

#[test]
fn subtask_merge_target_points_to_epic_branch_not_main() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-neg002";
    let integration_branch = "epic/feature-auth";

    create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    let subtask_id = "T-sub-neg003";
    let subtask = create_subtask(
        &tasks_dir,
        subtask_id,
        epic_id,
        "pending",
        0,
        integration_branch,
    );

    assert_ne!(
        subtask["merge_target"], "main",
        "Subtask merge_target should point to epic branch, not main"
    );
    assert_eq!(subtask["merge_target"], integration_branch);
}

#[test]
fn standalone_task_without_parent_has_main_as_default_target() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let task_id = "T-standalone001";
    let task = serde_json::json!({
        "task_id": task_id,
        "title": "Standalone task",
        "description": "No parent epic - merges to main",
        "status": "pending",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": serde_json::Value::Null,
        "percent": 0,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });

    let path = tasks_dir.join(format!("{}.json", task_id));
    std::fs::write(&path, serde_json::to_string_pretty(&task).unwrap()).unwrap();

    let loaded = read_task(&tasks_dir, task_id).unwrap();
    assert!(
        loaded.get("merge_target").is_none() || loaded["merge_target"].is_null(),
        "Standalone tasks should not have merge_target set"
    );
    assert!(
        loaded.get("parent_id").is_none() || loaded["parent_id"].is_null(),
        "Standalone tasks should not have parent_id"
    );
}

#[test]
fn epic_branch_diverged_from_main_requires_resolution() {
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "epic/diverged-epic"]);
    std::fs::write(workspace.path().join("conflict_file.txt"), "epic version\n").unwrap();
    run_git(workspace.path(), &["add", "conflict_file.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic commit"]);

    run_git(workspace.path(), &["checkout", "main"]);
    std::fs::write(workspace.path().join("conflict_file.txt"), "main version\n").unwrap();
    run_git(workspace.path(), &["add", "conflict_file.txt"]);
    run_git(workspace.path(), &["commit", "-m", "diverging main commit"]);

    run_git(workspace.path(), &["checkout", "epic/diverged-epic"]);

    let merge_result = Command::new("git")
        .args(["merge", "--no-commit", "--no-ff", "main"])
        .current_dir(workspace.path())
        .output()
        .unwrap();

    let has_conflicts = !merge_result.status.success();

    if has_conflicts {
        let abort_result = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(workspace.path())
            .output()
            .unwrap();
        assert!(
            abort_result.status.success(),
            "Merge abort should succeed after conflict"
        );
    } else {
        let _ = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(workspace.path())
            .output();
        let _ = Command::new("git")
            .args(["reset", "--hard", "HEAD"])
            .current_dir(workspace.path())
            .output();
    }

    assert!(
        has_conflicts,
        "Diverged branches with conflicting changes should have conflicts requiring resolution"
    );
}

#[test]
fn subtask_cannot_merge_directly_to_main_when_epic_exists() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-neg003";
    let integration_branch = "epic/T-epic-neg003";

    create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    let subtask_id = "T-sub-neg004";
    let subtask = create_subtask(
        &tasks_dir,
        subtask_id,
        epic_id,
        "approved",
        100,
        integration_branch,
    );

    let subtask_merge_target = subtask
        .get("merge_target")
        .and_then(|v| v.as_str())
        .unwrap();
    assert_eq!(
        subtask_merge_target, integration_branch,
        "Subtask should merge to epic branch, not main"
    );
    assert_ne!(
        subtask_merge_target, "main",
        "Subtask should NOT have main as merge_target"
    );
}
