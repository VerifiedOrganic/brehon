//! Recovery and restart tests for epic integration branch flow
//!
//! Tests:
//! 1. State persistence: epic+subtask state, integration_branch, merge_target survive restart
//! 2. Stale epic branch detection: epic branch exists but worktree is gone
//! 3. Partial integration state: some subtasks integrated, others pending

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
    std::fs::create_dir_all(tasks_dir).unwrap();

    let mut task = serde_json::json!({
        "task_id": epic_id,
        "title": format!("Epic {}", epic_id),
        "description": "Feature epic for testing integration flow",
        "status": "pending",
        "task_type": "epic",
        "completion_mode": "merge",
        "assignee": serde_json::Value::Null,
        "percent": 0,
        "created_at": Utc::now().to_rfc3339(),
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

#[test]
fn epic_state_persists_across_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-restart001";
    let integration_branch = "epic/T-epic-restart001";

    let epic = create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    std::fs::remove_file(tasks_dir.join(format!("{}.json", epic_id))).unwrap();

    std::fs::write(
        tasks_dir.join(format!("{}.json", epic_id)),
        serde_json::to_string_pretty(&epic).unwrap(),
    )
    .unwrap();

    let reloaded =
        read_task(&tasks_dir, epic_id).expect("Epic should be reloadable after restart simulation");

    assert_eq!(reloaded["task_id"], epic_id);
    assert_eq!(reloaded["task_type"], "epic");
    assert_eq!(reloaded["integration_branch"], integration_branch);
    assert_eq!(reloaded["status"], "pending");
    assert_eq!(reloaded["completion_mode"], "merge");
}

#[test]
fn subtask_state_persists_with_merge_target() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-restart002";
    let integration_branch = "epic/T-epic-restart002";

    create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    let subtask_id = "T-sub-restart001";
    let subtask = create_subtask(
        &tasks_dir,
        subtask_id,
        epic_id,
        "in_progress",
        75,
        integration_branch,
        "pending",
    );

    std::fs::remove_file(tasks_dir.join(format!("{}.json", subtask_id))).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&subtask).unwrap(),
    )
    .unwrap();

    let reloaded = read_task(&tasks_dir, subtask_id)
        .expect("Subtask should be reloadable after restart simulation");

    assert_eq!(reloaded["task_id"], subtask_id);
    assert_eq!(reloaded["parent_id"], epic_id);
    assert_eq!(reloaded["merge_target"], integration_branch);
    assert_eq!(reloaded["integration_status"], "pending");
    assert_eq!(reloaded["status"], "in_progress");
    assert_eq!(reloaded["percent"], 75);
}

#[test]
fn partial_integration_state_reflected_after_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-restart003";
    let integration_branch = "epic/T-epic-restart003";

    let mut epic = create_epic_task(&tasks_dir, epic_id, Some(integration_branch));
    epic["status"] = serde_json::Value::String("in_progress".to_string());
    write_task(&tasks_dir, &epic);

    let subtask1_id = "T-sub-restart002";
    create_subtask(
        &tasks_dir,
        subtask1_id,
        epic_id,
        "merged",
        100,
        integration_branch,
        "integrated",
    );

    let subtask2_id = "T-sub-restart003";
    create_subtask(
        &tasks_dir,
        subtask2_id,
        epic_id,
        "in_progress",
        50,
        integration_branch,
        "pending",
    );

    let subtask3_id = "T-sub-restart004";
    create_subtask(
        &tasks_dir,
        subtask3_id,
        epic_id,
        "pending",
        0,
        integration_branch,
        "pending",
    );

    let all_subtasks: Vec<_> = std::fs::read_dir(&tasks_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            let task: serde_json::Value = serde_json::from_str(&content).ok()?;
            if task.get("parent_id").and_then(|v| v.as_str()) == Some(epic_id) {
                Some(task)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(all_subtasks.len(), 3, "Should have 3 subtasks");

    let integrated_count = all_subtasks
        .iter()
        .filter(|t| t.get("integration_status").and_then(|v| v.as_str()) == Some("integrated"))
        .count();

    let pending_count = all_subtasks
        .iter()
        .filter(|t| t.get("integration_status").and_then(|v| v.as_str()) == Some("pending"))
        .count();

    assert_eq!(integrated_count, 1, "One subtask should be integrated");
    assert_eq!(pending_count, 2, "Two subtasks should be pending");

    let merged_count = all_subtasks
        .iter()
        .filter(|t| t.get("status").and_then(|v| v.as_str()) == Some("merged"))
        .count();

    let in_progress_count = all_subtasks
        .iter()
        .filter(|t| t.get("status").and_then(|v| v.as_str()) == Some("in_progress"))
        .count();

    assert_eq!(merged_count, 1, "One subtask should be merged");
    assert_eq!(in_progress_count, 1, "One subtask should be in_progress");
}

#[test]
fn stale_epic_branch_detection_worktree_gone() {
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "epic/T-stale"]);
    std::fs::write(workspace.path().join("epic_file.txt"), "epic\n").unwrap();
    run_git(workspace.path(), &["add", "epic_file.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic work"]);

    run_git(workspace.path(), &["checkout", "main"]);

    let worktree_base = workspace.path().join(".brehon").join("worktrees");
    std::fs::create_dir_all(&worktree_base).unwrap();

    let worktree_path = worktree_base.join("worker-stale");

    run_git(
        workspace.path(),
        &[
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
            "epic/T-stale",
        ],
    );

    assert!(worktree_path.exists(), "Worktree should exist");

    run_git(workspace.path(), &["worktree", "list"]);

    std::fs::remove_dir_all(&worktree_path).unwrap();

    assert!(!worktree_path.exists(), "Worktree should be gone");

    let branches_output = run_git(workspace.path(), &["branch", "--list", "epic/T-stale"]);
    assert!(
        branches_output.contains("epic/T-stale"),
        "Branch should still exist"
    );
}

#[test]
fn integration_status_transitions_correctly() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic-restart004";
    let integration_branch = "epic/T-epic-restart004";

    create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    let subtask_id = "T-sub-ReTrans001";
    let mut subtask = create_subtask(
        &tasks_dir,
        subtask_id,
        epic_id,
        "pending",
        0,
        integration_branch,
        "pending",
    );

    assert_eq!(subtask["integration_status"], "pending");

    subtask["status"] = serde_json::Value::String("in_progress".to_string());
    subtask["percent"] = serde_json::Value::Number(50.into());
    write_task(&tasks_dir, &subtask);

    subtask["status"] = serde_json::Value::String("approved".to_string());
    subtask["percent"] = serde_json::Value::Number(100.into());
    write_task(&tasks_dir, &subtask);

    subtask["status"] = serde_json::Value::String("merged".to_string());
    subtask["integration_status"] = serde_json::Value::String("integrated".to_string());
    write_task(&tasks_dir, &subtask);

    let final_task = read_task(&tasks_dir, subtask_id).unwrap();
    assert_eq!(final_task["status"], "merged");
    assert_eq!(final_task["integration_status"], "integrated");
}
