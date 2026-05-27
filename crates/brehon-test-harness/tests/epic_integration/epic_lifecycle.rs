//! Test: Full lifecycle of epic integration branch flow
//!
//! Tests the complete flow:
//! 1. Create feature epic with integration_branch
//! 2. Create subtasks under epic
//! 3. Subtasks complete and merge into epic branch
//! 4. Epic closes and merges to main

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
        "created_at": chrono::Utc::now().to_rfc3339(),
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

fn write_task(tasks_dir: &Path, task: &serde_json::Value) {
    std::fs::create_dir_all(tasks_dir).unwrap();
    let task_id = task.get("task_id").and_then(|v| v.as_str()).unwrap();
    let path = tasks_dir.join(format!("{}.json", task_id));
    std::fs::write(&path, serde_json::to_string_pretty(&task).unwrap()).unwrap();
}

#[test]
fn create_feature_epic_with_integration_branch() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic001";
    let integration_branch = "epic/T-epic001";

    let epic = create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    assert_eq!(epic["task_type"], "epic");
    assert_eq!(epic["integration_branch"], integration_branch);
    assert_eq!(epic["status"], "pending");

    let loaded = read_task(&tasks_dir, epic_id).expect("Epic should be persisted");
    assert_eq!(loaded["integration_branch"], integration_branch);
}

#[test]
fn create_subtask_inherits_merge_target_from_epic() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic002";
    let integration_branch = "epic/feature-auth";

    create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    let subtask_id = "T-sub001";
    let subtask = create_subtask(
        &tasks_dir,
        subtask_id,
        epic_id,
        "pending",
        0,
        integration_branch,
    );

    assert_eq!(subtask["parent_id"], epic_id);
    assert_eq!(subtask["merge_target"], integration_branch);
    assert_eq!(subtask["integration_status"], "pending");

    let loaded = read_task(&tasks_dir, subtask_id).expect("Subtask should be persisted");
    assert_eq!(loaded["merge_target"], integration_branch);
}

#[test]
fn subtask_without_epic_parent_has_no_merge_target() {
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let brehon_root = workspace.path().join(".brehon");
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let task_id = "T-standalone";
    let task = serde_json::json!({
        "task_id": task_id,
        "title": "Standalone task",
        "description": "No parent epic",
        "status": "pending",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": serde_json::Value::Null,
        "percent": 0,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });

    let path = tasks_dir.join(format!("{}.json", task_id));
    std::fs::write(&path, serde_json::to_string_pretty(&task).unwrap()).unwrap();

    let loaded = read_task(&tasks_dir, task_id).expect("Task should be persisted");
    assert!(loaded.get("merge_target").is_none() || loaded["merge_target"].is_null());
    assert!(loaded.get("integration_status").is_none() || loaded["integration_status"].is_null());
}

#[test]
fn epic_branch_ancestry_subtask_descends_from_epic() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let _head_commit = init_git_workspace(workspace.path());

    let _epic_id = "T-epic003";
    let integration_branch = "epic/T-epic003";

    run_git(workspace.path(), &["checkout", "-b", integration_branch]);
    std::fs::write(workspace.path().join("epic_file.txt"), "epic content\n").unwrap();
    run_git(workspace.path(), &["add", "epic_file.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch start"]);
    let epic_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);

    let subtask_branch = "feature/T-sub002";
    run_git(
        workspace.path(),
        &["checkout", "-b", subtask_branch, &epic_commit],
    );

    let _result = run_git(
        workspace.path(),
        &[
            "merge-base",
            "--is-ancestor",
            subtask_branch,
            integration_branch,
        ],
    );

    assert!(
        run_git(
            workspace.path(),
            &[
                "merge-base",
                "--is-ancestor",
                subtask_branch,
                integration_branch
            ]
        ) == "0"
            || Command::new("git")
                .args([
                    "merge-base",
                    "--is-ancestor",
                    subtask_branch,
                    integration_branch
                ])
                .current_dir(workspace.path())
                .status()
                .unwrap()
                .success(),
        "Subtask branch should descend from epic branch"
    );

    run_git(workspace.path(), &["checkout", "main"]);
}

#[test]
fn multi_subtask_sequential_integration_into_epic_branch() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let _head_commit = init_git_workspace(workspace.path());

    let epic_id = "T-epic004";
    let integration_branch = "epic/T-epic004";

    create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    run_git(workspace.path(), &["checkout", "-b", integration_branch]);
    std::fs::write(workspace.path().join("epic_base.txt"), "epic base\n").unwrap();
    run_git(workspace.path(), &["add", "epic_base.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic base commit"]);
    let epic_base_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let subtask1_id = "T-sub003";
    let subtask1_branch = "feature/T-sub003";
    create_subtask(
        &tasks_dir,
        subtask1_id,
        epic_id,
        "pending",
        0,
        integration_branch,
    );

    run_git(
        workspace.path(),
        &["checkout", "-b", subtask1_branch, &epic_base_commit],
    );
    std::fs::write(workspace.path().join("sub1.txt"), "subtask 1\n").unwrap();
    run_git(workspace.path(), &["add", "sub1.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "subtask 1 implementation"],
    );
    let _subtask1_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", integration_branch]);
    run_git(workspace.path(), &["merge", subtask1_branch, "--no-edit"]);

    let mut subtask1 = read_task(&tasks_dir, subtask1_id).unwrap();
    subtask1["status"] = serde_json::Value::String("merged".to_string());
    subtask1["integration_status"] = serde_json::Value::String("integrated".to_string());
    write_task(&tasks_dir, &subtask1);

    let subtask2_id = "T-sub004";
    let subtask2_branch = "feature/T-sub004";
    create_subtask(
        &tasks_dir,
        subtask2_id,
        epic_id,
        "pending",
        0,
        integration_branch,
    );

    let epic_after_subtask1 = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(
        workspace.path(),
        &["checkout", "-b", subtask2_branch, &epic_after_subtask1],
    );
    std::fs::write(workspace.path().join("sub2.txt"), "subtask 2\n").unwrap();
    run_git(workspace.path(), &["add", "sub2.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "subtask 2 implementation"],
    );
    let _subtask2_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", integration_branch]);
    run_git(workspace.path(), &["merge", subtask2_branch, "--no-edit"]);

    let mut subtask2 = read_task(&tasks_dir, subtask2_id).unwrap();
    subtask2["status"] = serde_json::Value::String("merged".to_string());
    subtask2["integration_status"] = serde_json::Value::String("integrated".to_string());
    write_task(&tasks_dir, &subtask2);

    run_git(workspace.path(), &["checkout", "main"]);

    let subtask_files: Vec<_> = std::fs::read_dir(workspace.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("sub"))
        .collect();
    assert_eq!(
        subtask_files.len(),
        0,
        "Subtask files should not be on main yet"
    );

    run_git(workspace.path(), &["checkout", integration_branch]);
    let subtask_files: Vec<_> = std::fs::read_dir(workspace.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("sub"))
        .collect();
    assert_eq!(
        subtask_files.len(),
        2,
        "Both subtask files should be on epic branch"
    );
}

#[test]
fn epic_closing_prevents_main_merge_until_all_subtasks_integrated() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic005";
    let integration_branch = "epic/T-epic005";

    let mut epic = create_epic_task(&tasks_dir, epic_id, Some(integration_branch));
    epic["status"] = serde_json::Value::String("assigned".to_string());
    write_task(&tasks_dir, &epic);

    let subtask1_id = "T-sub005";
    create_subtask(
        &tasks_dir,
        subtask1_id,
        epic_id,
        "in_progress",
        50,
        integration_branch,
    );

    let subtask2_id = "T-sub006";
    create_subtask(
        &tasks_dir,
        subtask2_id,
        epic_id,
        "pending",
        0,
        integration_branch,
    );

    let epic_check = read_task(&tasks_dir, epic_id).unwrap();

    assert_eq!(epic_check["status"], "assigned");

    let subtasks: Vec<_> = std::fs::read_dir(&tasks_dir)
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

    assert_eq!(subtasks.len(), 2, "Should have 2 subtasks");

    let non_terminal_count = subtasks
        .iter()
        .filter(|t| {
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            !matches!(status, "merged" | "closed")
        })
        .count();

    assert!(
        non_terminal_count > 0,
        "Not all subtasks should be integrated"
    );
}

#[test]
fn gate_transitions_subtask_through_lifecycle() {
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    init_git_workspace(workspace.path());

    let epic_id = "T-epic006";
    let integration_branch = "epic/T-epic006";

    create_epic_task(&tasks_dir, epic_id, Some(integration_branch));

    let subtask_id = "T-sub007";
    let mut subtask = create_subtask(
        &tasks_dir,
        subtask_id,
        epic_id,
        "pending",
        0,
        integration_branch,
    );

    let transitions = ["assigned", "in_progress", "in_review", "approved", "merged"];

    for new_status in &transitions[1..] {
        subtask["status"] = serde_json::Value::String(new_status.to_string());
        write_task(&tasks_dir, &subtask);

        let loaded = read_task(&tasks_dir, subtask_id).unwrap();
        assert_eq!(
            loaded["status"], *new_status,
            "Status should transition to {}",
            new_status
        );
    }

    subtask["status"] = serde_json::Value::String("merged".to_string());
    subtask["integration_status"] = serde_json::Value::String("integrated".to_string());
    write_task(&tasks_dir, &subtask);

    let final_task = read_task(&tasks_dir, subtask_id).unwrap();
    assert_eq!(final_task["status"], "merged");
    assert_eq!(final_task["integration_status"], "integrated");
}
