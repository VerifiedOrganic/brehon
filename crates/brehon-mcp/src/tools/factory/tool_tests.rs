//! Tests for the factory tool.

use super::paths::read_task;
use super::tool::FactoryTool;
use crate::server::ContentBlock;
use crate::tools::{ScopedEnv, Tool, TEST_ENV_LOCK};
use brehon_mux::PromptQueueEntry;
use serde_json::Value;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt};
use tempfile::TempDir;

struct ScopedCurrentDir {
    saved: PathBuf,
}

impl ScopedCurrentDir {
    fn set(path: &Path) -> Self {
        let saved = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(path).expect("set current dir");
        Self { saved }
    }
}

impl Drop for ScopedCurrentDir {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.saved).expect("restore current dir");
    }
}

fn extract_text(result: &crate::server::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn make_test_root() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn write_test_task(root: &Path, task_id: &str, status: &str, task_type: &str) {
    write_test_task_with_assignee(root, task_id, status, task_type, Value::Null);
}

fn write_test_task_with_assignee(
    root: &Path,
    task_id: &str,
    status: &str,
    task_type: &str,
    assignee: Value,
) {
    let tasks_dir = root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let task = serde_json::json!({
        "task_id": task_id,
        "title": format!("Task {task_id}"),
        "status": status,
        "task_type": task_type,
        "assignee": assignee
    });
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
}

fn write_test_task_with_policy(root: &Path, task_id: &str, execution_policy: Value) {
    let tasks_dir = root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let task = serde_json::json!({
        "task_id": task_id,
        "title": format!("Task {task_id}"),
        "status": "pending",
        "task_type": "task",
        "assignee": null,
        "execution_policy": execution_policy
    });
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
}

fn read_test_task(root: &Path, task_id: &str) -> Value {
    let path = root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn write_test_task_json(root: &Path, task_id: &str, task: &Value) {
    let tasks_dir = root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(task).unwrap(),
    )
    .unwrap();
}

fn write_routing_overlay(project_root: &Path) {
    let brehon_dir = project_root.join(".brehon");
    std::fs::create_dir_all(&brehon_dir).unwrap();
    std::fs::write(
        brehon_dir.join("config.yaml"),
        r#"
roles:
  workers:
    - lane: kimi-worker
      min: 1
      max: 1
    - lane: gpt53-worker
      min: 1
      max: 1
routing:
  default_worker_lane: kimi-worker
  rules:
    - id: high-risk-release
      match:
        text_any:
          - release
          - supply-chain
      policy:
        preferred_lane: gpt53-worker
        preferred_model: gpt-5.3
        strict: true
"#,
    )
    .unwrap();
}

fn write_invalid_routing_overlay(project_root: &Path) {
    let brehon_dir = project_root.join(".brehon");
    std::fs::create_dir_all(&brehon_dir).unwrap();
    std::fs::write(
        brehon_dir.join("config.yaml"),
        r#"
routing:
  default_worker_lane: missing-worker
"#,
    )
    .unwrap();
}

fn run_git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {}: {}", args.join(" "), e));
    assert!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn make_git_repo_with_brehon_root() -> TempDir {
    let repo_root = tempfile::tempdir().unwrap();
    run_git(repo_root.path(), &["init"]);
    run_git(repo_root.path(), &["branch", "-M", "main"]);
    run_git(repo_root.path(), &["config", "user.name", "Test"]);
    run_git(
        repo_root.path(),
        &["config", "user.email", "test@example.com"],
    );
    std::fs::create_dir_all(
        repo_root
            .path()
            .join(".brehon")
            .join("runtime")
            .join("tasks"),
    )
    .unwrap();
    std::fs::write(repo_root.path().join("README.md"), "base\n").unwrap();
    run_git(repo_root.path(), &["add", "README.md"]);
    run_git(repo_root.path(), &["commit", "-m", "initial"]);
    repo_root
}

fn create_worker_worktree(repo_root: &Path, worker_name: &str, branch: &str) -> PathBuf {
    run_git(repo_root, &["branch", branch]);
    let worktree_path = repo_root
        .join(".brehon")
        .join("worktrees")
        .join("runs")
        .join("run-1")
        .join(worker_name);
    std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
    let repo = git2::Repository::open(repo_root).unwrap();
    brehon_git::WorktreeOps::new(&repo)
        .create_worktree(branch, &worktree_path)
        .unwrap();
    worktree_path
}

fn create_epic_worktree(repo_root: &Path, epic_id: &str, branch: &str) -> PathBuf {
    run_git(repo_root, &["branch", branch]);
    let worktree_path = repo_root
        .join(".brehon")
        .join("worktrees")
        .join("epic")
        .join(epic_id);
    std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
    let repo = git2::Repository::open(repo_root).unwrap();
    brehon_git::WorktreeOps::new(&repo)
        .create_worktree(branch, &worktree_path)
        .unwrap();
    worktree_path
}

fn create_initiative_worktree(repo_root: &Path, initiative_id: &str, branch: &str) -> PathBuf {
    run_git(repo_root, &["branch", branch]);
    let worktree_path = repo_root
        .join(".brehon")
        .join("worktrees")
        .join("initiative")
        .join(initiative_id);
    std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();
    let repo = git2::Repository::open(repo_root).unwrap();
    brehon_git::WorktreeOps::new(&repo)
        .create_worktree(branch, &worktree_path)
        .unwrap();
    worktree_path
}

fn write_task_json(brehon_root: &Path, task: Value) {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let task_id = task["task_id"].as_str().unwrap();
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn test_spawn_workers() {
    let tool = FactoryTool::new();
    let args = serde_json::json!({
        "action": "spawn_workers",
        "count": 3
    });
    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());
    if let ContentBlock::Text { text } = &result.content[0] {
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["count"], 3);
    }
}

#[tokio::test]
async fn test_spawn_alias_maps_to_spawn_workers() {
    let tool = FactoryTool::new();
    let args = serde_json::json!({
        "action": "spawn",
        "count": 2
    });
    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());
    if let ContentBlock::Text { text } = &result.content[0] {
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["count"], 2);
    }
}

#[tokio::test]
async fn test_help_action_returns_supported_actions() {
    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "help" }))
        .await
        .unwrap();
    assert!(result.is_error.is_none());
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(v["status"], "ok");
    assert!(v["actions"]
        .as_array()
        .unwrap()
        .contains(&Value::String("assign_workers".into())));
    assert_eq!(v["aliases"]["dispatch"], "assign_workers");
}

#[tokio::test]
async fn test_worker_status() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    create_worker_worktree(repo_root.path(), "worker-2", "worker-2-branch");
    write_test_task_with_assignee(
        &brehon_root,
        "T-busy",
        "in_review",
        "task",
        Value::String("worker-1".to_string()),
    );
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );

    let tool = FactoryTool::new();
    let args = serde_json::json!({ "action": "worker_status" });
    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert!(v["workers"].is_array());
    assert_eq!(v["busy_worker_count"], 1);
    assert_eq!(v["idle_worker_count"], 1);

    let workers = v["workers"].as_array().unwrap();
    let busy_worker = workers
        .iter()
        .find(|worker| worker["name"] == "worker-1")
        .expect("worker-1 missing");
    let availability = busy_worker["availability"].as_str().unwrap();
    assert!(
        availability.contains("busy"),
        "expected busy availability, got: {availability}"
    );
    assert!(
        availability.contains("T-busy"),
        "expected task id in availability, got: {availability}"
    );
    assert!(
        availability.contains("in_review"),
        "expected in_review in availability, got: {availability}"
    );
    assert_eq!(busy_worker["active_task_count"], 1);
    assert_eq!(busy_worker["active_tasks"].as_array().unwrap().len(), 1);

    let idle_worker = workers
        .iter()
        .find(|worker| worker["name"] == "worker-2")
        .expect("worker-2 missing");
    assert_eq!(idle_worker["availability"], "idle");
    assert_eq!(idle_worker["active_task_count"], 0);
}

#[tokio::test]
async fn test_worker_status_reports_reserved_lane_metadata() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    create_worker_worktree(repo_root.path(), "hardener-1", "hardener-branch");
    crate::tools::agent::write_session_file_with_metadata(
        "hardener-1",
        "worker",
        "hardener-session",
        Some("codex-hardening"),
        Some("gpt-5.5"),
        Some("xhigh"),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "worker_status" }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let workers = v["workers"].as_array().unwrap();
    let hardener = workers
        .iter()
        .find(|worker| worker["name"] == "hardener-1")
        .expect("hardener missing");
    assert_eq!(hardener["assignment_mode"], "reserved");
    assert_eq!(hardener["reserved"], true);
    assert_eq!(hardener["available_for_assignment"], true);
    assert_eq!(hardener["available_for_general_assignment"], false);
    assert!(hardener["accepted_work_classes"]
        .as_array()
        .unwrap()
        .contains(&Value::String("final_hardening".to_string())));
    assert_eq!(v["idle_general_worker_count"], 0);
}

#[tokio::test]
async fn test_worker_status_missing_worktree_is_not_assignable() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "unregistered-worker",
        "worker",
        "unregistered-session",
        Some("opencode"),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "worker_status" }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let workers = v["workers"].as_array().unwrap();
    let worker = workers
        .iter()
        .find(|worker| worker["name"] == "unregistered-worker")
        .expect("worker missing");
    assert_eq!(worker["availability"], "unavailable (missing worktree)");
    assert_eq!(worker["available_for_assignment"], false);
    assert_eq!(worker["available_for_general_assignment"], false);
    assert_eq!(worker["worktree"]["worktree_exists"], false);
    assert_eq!(v["idle_general_worker_count"], 0);
}

#[tokio::test]
async fn test_assign_workers_missing_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = FactoryTool::new();
    let args = serde_json::json!({
        "action": "assign_workers",
        "workers": "worker-1,worker-2"
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_assign_alias_maps_to_assign_workers() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task(root.path(), "T-alias-assign", "pending", "task");
    crate::tools::agent::write_session_file(
        "worker-alias",
        "worker",
        "worker-alias-session",
        Some("codex"),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign",
            "task_id": "T-alias-assign",
            "worker": "worker-alias"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));

    let task = read_task("T-alias-assign").expect("task should exist");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-alias");
}

#[tokio::test]
async fn test_assign_workers_missing_workers() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = FactoryTool::new();
    let args = serde_json::json!({
        "action": "assign_workers",
        "task_id": "T-nonexistent"
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_set_ownership_missing_params() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = FactoryTool::new();
    let args = serde_json::json!({
        "action": "set_ownership",
        "worker": "worker-1"
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_unknown_action() {
    let tool = FactoryTool::new();
    let args = serde_json::json!({ "action": "bogus" });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_assign_workers_requires_supervisor() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task(root.path(), "T-pending", "pending", "task");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-pending",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("Only supervisors can assign workers"));

    let task = read_test_task(root.path(), "T-pending");
    assert_eq!(task["status"], "pending");
    assert!(task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_moves_pending_task_to_assigned() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task(root.path(), "T-pending", "pending", "task");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-pending",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let task = read_test_task(root.path(), "T-pending");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-1");
}

#[tokio::test]
async fn test_assign_workers_rejects_regular_task_on_reserved_worker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task(root.path(), "T-regular", "pending", "task");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file_with_metadata(
        "hardener-1",
        "worker",
        "hardener-session",
        Some("codex-hardening"),
        Some("gpt-5.5"),
        Some("xhigh"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-regular",
            "worker": "hardener-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("reserved for final_hardening"), "{text}");
    let task = read_test_task(root.path(), "T-regular");
    assert_eq!(task["status"], "pending");
    assert!(task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_allows_final_hardening_task_on_reserved_worker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task_with_policy(
        root.path(),
        "T-hardening",
        serde_json::json!({
            "work_class": "final_hardening",
            "preferred_lane": "codex-hardening",
            "preferred_model": "gpt-5.5",
            "preferred_reasoning_effort": "xhigh",
            "strict": true
        }),
    );
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file_with_metadata(
        "hardener-1",
        "worker",
        "hardener-session",
        Some("codex-hardening"),
        Some("gpt-5.5"),
        Some("xhigh"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-hardening",
            "worker": "hardener-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let task = read_test_task(root.path(), "T-hardening");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "hardener-1");
}

#[tokio::test]
async fn test_assign_workers_rejects_strict_preferred_lane_mismatch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task_with_policy(
        root.path(),
        "T-hardening",
        serde_json::json!({
            "work_class": "final_hardening",
            "preferred_lane": "codex-hardening",
            "strict": true
        }),
    );
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file_with_metadata(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("codex-worker"),
        Some("gpt-5.3-codex"),
        Some("medium"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-hardening",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("preferred_lane='codex-hardening'"), "{text}");
    let task = read_test_task(root.path(), "T-hardening");
    assert_eq!(task["status"], "pending");
    assert!(task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_cannot_force_final_hardening_to_wrong_lane() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task_with_policy(
        root.path(),
        "T-hardening",
        serde_json::json!({
            "work_class": "final_hardening",
            "preferred_lane": "codex-hardening",
            "preferred_model": "gpt-5.5",
            "preferred_reasoning_effort": "xhigh",
            "strict": true
        }),
    );
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file_with_metadata(
        "kimi-worker",
        "worker",
        "kimi-session",
        Some("kimi-worker-kimi-for-coding"),
        Some("kimi-for-coding"),
        Some("high"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-hardening",
            "worker": "kimi-worker",
            "force_policy": true
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("Cannot force assign task T-hardening"),
        "{text}"
    );
    assert!(text.contains("strict final_hardening"), "{text}");
    assert!(text.contains("preferred_lane='codex-hardening'"), "{text}");
    let task = read_test_task(root.path(), "T-hardening");
    assert_eq!(task["status"], "pending");
    assert!(task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_rejects_invalid_project_routing_config() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = make_test_root();
    write_invalid_routing_overlay(project.path());
    let brehon_root = project.path().join(".brehon");
    let config_dir = project.path().join("xdg");
    write_test_task(&brehon_root, "T-pending", "pending", "task");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("XDG_CONFIG_HOME", config_dir.to_str().unwrap()),
    ]);
    crate::tools::agent::write_session_file_with_metadata(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("kimi-worker"),
        Some("kimi-k2.6"),
        None,
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-pending",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("Project config is invalid"),
        "expected invalid config error, got: {text}"
    );
    assert!(
        text.contains("routing.default_worker_lane"),
        "expected routing lane detail, got: {text}"
    );
    let task = read_test_task(&brehon_root, "T-pending");
    assert_eq!(task["status"], "pending");
    assert!(task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_applies_config_routing_policy() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = make_test_root();
    write_routing_overlay(project.path());
    let brehon_root = project.path().join(".brehon");
    write_test_task(&brehon_root, "T-release", "pending", "task");
    let mut task = read_test_task(&brehon_root, "T-release");
    task["title"] = Value::String("Release supply-chain evidence".to_string());
    write_test_task_json(&brehon_root, "T-release", &task);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file_with_metadata(
        "kimi-1",
        "worker",
        "kimi-session",
        Some("kimi-worker"),
        Some("kimi-k2.6"),
        None,
    );
    crate::tools::agent::write_session_file_with_metadata(
        "gpt53-1",
        "worker",
        "gpt53-session",
        Some("gpt53-worker"),
        Some("gpt-5.3"),
        None,
    );
    let tool = FactoryTool::new();

    let rejected = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-release",
            "worker": "kimi-1"
        }))
        .await
        .unwrap();
    assert_eq!(rejected.is_error, Some(true));
    let text = extract_text(&rejected);
    assert!(text.contains("preferred_lane='gpt53-worker'"), "{text}");

    let accepted = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-release",
            "worker": "gpt53-1"
        }))
        .await
        .unwrap();
    assert!(accepted.is_error.is_none(), "{}", extract_text(&accepted));
    let payload: Value = serde_json::from_str(&extract_text(&accepted)).unwrap();
    assert_eq!(payload["routing"]["source"], "routing_rule");
    assert_eq!(payload["routing"]["rule_id"], "high-risk-release");
    assert_eq!(
        payload["routing"]["effective_execution_policy"]["preferred_lane"],
        "gpt53-worker"
    );
    let task = read_test_task(&brehon_root, "T-release");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "gpt53-1");
}

#[tokio::test]
async fn test_assign_workers_explicit_task_policy_wins_over_config_routing() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = make_test_root();
    write_routing_overlay(project.path());
    let brehon_root = project.path().join(".brehon");
    write_test_task_with_policy(
        &brehon_root,
        "T-release",
        serde_json::json!({
            "preferred_lane": "kimi-worker",
            "preferred_model": "kimi-k2.6",
            "strict": true
        }),
    );
    let mut task = read_test_task(&brehon_root, "T-release");
    task["title"] = Value::String("Release supply-chain evidence".to_string());
    write_test_task_json(&brehon_root, "T-release", &task);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file_with_metadata(
        "kimi-1",
        "worker",
        "kimi-session",
        Some("kimi-worker"),
        Some("kimi-k2.6"),
        None,
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-release",
            "worker": "kimi-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["routing"]["source"], "task");
    assert_eq!(
        payload["routing"]["effective_execution_policy"]["preferred_lane"],
        "kimi-worker"
    );
    let task = read_test_task(&brehon_root, "T-release");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "kimi-1");
}

#[tokio::test]
async fn test_assign_workers_syncs_worker_branch_to_merge_target() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    let worker_branch = "brehon/runs/run-1/worker-1";
    let worktree_path = create_worker_worktree(repo_root.path(), "worker-1", worker_branch);

    run_git(repo_root.path(), &["checkout", "-b", "epic/test"]);
    std::fs::write(repo_root.path().join("phase.txt"), "integrated change\n").unwrap();
    run_git(repo_root.path(), &["add", "phase.txt"]);
    run_git(repo_root.path(), &["commit", "-m", "epic change"]);
    let merge_target_head = run_git(repo_root.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "T-merge-target",
            "title": "Merge target task",
            "status": "pending",
            "task_type": "task",
            "assignee": null,
            "merge_target": "epic/test"
        }),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-merge-target",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["merge_target_sync"]["status"], "reset");
    assert_eq!(json["merge_target_sync"]["merge_target"], "epic/test");

    let worker_head = run_git(&worktree_path, &["rev-parse", "HEAD"]);
    assert_eq!(worker_head, merge_target_head);
    let task = read_test_task(&brehon_root, "T-merge-target");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-1");
}

#[tokio::test]
async fn test_assign_workers_rejects_dirty_worker_worktree_for_merge_target_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    let worker_branch = "brehon/runs/run-1/worker-1";
    let worktree_path = create_worker_worktree(repo_root.path(), "worker-1", worker_branch);

    run_git(repo_root.path(), &["checkout", "-b", "epic/test"]);
    std::fs::write(repo_root.path().join("phase.txt"), "integrated change\n").unwrap();
    run_git(repo_root.path(), &["add", "phase.txt"]);
    run_git(repo_root.path(), &["commit", "-m", "epic change"]);

    std::fs::write(worktree_path.join("local.txt"), "dirty\n").unwrap();

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "T-dirty-merge-target",
            "title": "Dirty merge target task",
            "status": "pending",
            "task_type": "task",
            "assignee": null,
            "merge_target": "epic/test"
        }),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-dirty-merge-target",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("worktree"));
    assert!(extract_text(&result).contains("dirty"));
    let task = read_test_task(&brehon_root, "T-dirty-merge-target");
    assert_eq!(task["status"], "pending");
    assert!(task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_syncs_epic_merge_target_branch_from_default_before_worker_sync() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    std::fs::write(
        repo_root.path().join(".git").join("info").join("exclude"),
        ".brehon/worktrees\n",
    )
    .unwrap();
    let epic_branch = "epic/phase-2";
    let epic_worktree = create_epic_worktree(repo_root.path(), "E-phase-2", epic_branch);
    let worker_branch = "brehon/runs/run-1/worker-1";
    let worker_worktree = create_worker_worktree(repo_root.path(), "worker-1", worker_branch);

    std::fs::write(repo_root.path().join("upstream.txt"), "landed on main\n").unwrap();
    run_git(repo_root.path(), &["add", "upstream.txt"]);
    run_git(repo_root.path(), &["commit", "-m", "phase 1 landed"]);
    let main_head = run_git(repo_root.path(), &["rev-parse", "HEAD"]);

    let epic_head_before = run_git(&epic_worktree, &["rev-parse", "HEAD"]);
    assert_ne!(
        epic_head_before, main_head,
        "epic branch should start stale"
    );
    let worker_head_before = run_git(&worker_worktree, &["rev-parse", "HEAD"]);
    assert_ne!(
        worker_head_before, main_head,
        "worker branch should start stale"
    );

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "E-phase-2",
            "title": "Phase 2 Epic",
            "status": "pending",
            "task_type": "epic",
            "integration_branch": epic_branch,
            "integration_worktree": epic_worktree,
        }),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "T-phase-2",
            "title": "Phase 2 Task",
            "status": "pending",
            "task_type": "task",
            "parent_id": "E-phase-2",
            "assignee": null,
            "merge_target": epic_branch
        }),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-phase-2",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["merge_target_base_sync"]["status"], "merged");
    assert_eq!(json["merge_target_base_sync"]["merge_target"], epic_branch);
    assert_eq!(json["merge_target_base_sync"]["base_branch"], "main");
    assert_eq!(json["merge_target_sync"]["status"], "reset");

    let epic_head_after = run_git(&epic_worktree, &["rev-parse", "HEAD"]);
    let worker_head_after = run_git(&worker_worktree, &["rev-parse", "HEAD"]);
    assert_eq!(epic_head_after, main_head);
    assert_eq!(worker_head_after, main_head);
}

#[tokio::test]
async fn test_assign_workers_syncs_epic_merge_target_branch_from_initiative_branch_before_worker_sync(
) {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    std::fs::write(
        repo_root.path().join(".git").join("info").join("exclude"),
        ".brehon/worktrees\n",
    )
    .unwrap();
    let initiative_branch = "initiative/program";
    let initiative_worktree =
        create_initiative_worktree(repo_root.path(), "I-program", initiative_branch);
    let epic_branch = "epic/phase-2";
    let epic_worktree = create_epic_worktree(repo_root.path(), "E-phase-2", epic_branch);
    let worker_branch = "brehon/runs/run-1/worker-1";
    let worker_worktree = create_worker_worktree(repo_root.path(), "worker-1", worker_branch);

    run_git(&epic_worktree, &["reset", "--hard", initiative_branch]);
    run_git(&worker_worktree, &["reset", "--hard", epic_branch]);

    std::fs::write(
        initiative_worktree.join("initiative.txt"),
        "initiative landed\n",
    )
    .unwrap();
    run_git(&initiative_worktree, &["add", "initiative.txt"]);
    run_git(
        &initiative_worktree,
        &["commit", "-m", "initiative progress"],
    );
    let initiative_head = run_git(&initiative_worktree, &["rev-parse", "HEAD"]);

    let epic_head_before = run_git(&epic_worktree, &["rev-parse", "HEAD"]);
    let worker_head_before = run_git(&worker_worktree, &["rev-parse", "HEAD"]);
    assert_ne!(epic_head_before, initiative_head);
    assert_ne!(worker_head_before, initiative_head);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "I-program",
            "title": "Program",
            "status": "pending",
            "task_type": "initiative",
            "integration_branch": initiative_branch,
            "integration_worktree": initiative_worktree,
        }),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "E-phase-2",
            "title": "Phase 2 Epic",
            "status": "pending",
            "task_type": "epic",
            "parent_id": "I-program",
            "integration_branch": epic_branch,
            "integration_worktree": epic_worktree,
        }),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "T-phase-2",
            "title": "Phase 2 Task",
            "status": "pending",
            "task_type": "task",
            "parent_id": "E-phase-2",
            "assignee": null,
            "merge_target": epic_branch
        }),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-phase-2",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["merge_target_base_sync"]["status"], "merged");
    assert_eq!(json["merge_target_base_sync"]["merge_target"], epic_branch);
    assert_eq!(
        json["merge_target_base_sync"]["base_branch"],
        initiative_branch
    );
    assert_eq!(json["merge_target_sync"]["status"], "reset");

    let epic_head_after = run_git(&epic_worktree, &["rev-parse", "HEAD"]);
    let worker_head_after = run_git(&worker_worktree, &["rev-parse", "HEAD"]);
    assert_eq!(epic_head_after, initiative_head);
    assert_eq!(worker_head_after, initiative_head);
}

#[tokio::test]
async fn test_assign_workers_resolves_relative_epic_integration_worktree_from_non_root_cwd() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    std::fs::write(
        repo_root.path().join(".git").join("info").join("exclude"),
        ".brehon/worktrees\n",
    )
    .unwrap();

    let epic_branch = "epic/phase-0";
    let epic_worktree = create_epic_worktree(repo_root.path(), "E-phase-0", epic_branch);
    let worker_branch = "brehon/runs/run-1/worker-1";
    let worker_worktree = create_worker_worktree(repo_root.path(), "worker-1", worker_branch);

    std::fs::write(repo_root.path().join("upstream.txt"), "landed on main\n").unwrap();
    run_git(repo_root.path(), &["add", "upstream.txt"]);
    run_git(repo_root.path(), &["commit", "-m", "phase 0 landed"]);
    let main_head = run_git(repo_root.path(), &["rev-parse", "HEAD"]);

    let relative_epic_worktree = format!(
        "./{}",
        epic_worktree
            .strip_prefix(repo_root.path())
            .unwrap()
            .display()
    );

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let _cwd = ScopedCurrentDir::set(&worker_worktree);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "E-phase-0",
            "title": "Phase 0 Epic",
            "status": "pending",
            "task_type": "epic",
            "integration_branch": epic_branch,
            "integration_worktree": relative_epic_worktree,
        }),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "T-phase-0",
            "title": "Phase 0 Task",
            "status": "pending",
            "task_type": "task",
            "parent_id": "E-phase-0",
            "assignee": null,
            "merge_target": epic_branch
        }),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-phase-0",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let epic_head_after = run_git(&epic_worktree, &["rev-parse", "HEAD"]);
    let worker_head_after = run_git(&worker_worktree, &["rev-parse", "HEAD"]);
    assert_eq!(epic_head_after, main_head);
    assert_eq!(worker_head_after, main_head);
}

#[tokio::test]
async fn test_assign_workers_resets_contaminated_worker_branch_and_preserves_old_head() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    let worker_branch = "brehon/runs/run-1/worker-1";
    let worktree_path = create_worker_worktree(repo_root.path(), "worker-1", worker_branch);

    run_git(repo_root.path(), &["checkout", "-b", "epic/test"]);
    std::fs::write(repo_root.path().join("phase.txt"), "integrated change\n").unwrap();
    run_git(repo_root.path(), &["add", "phase.txt"]);
    run_git(repo_root.path(), &["commit", "-m", "epic change"]);
    let merge_target_head = run_git(repo_root.path(), &["rev-parse", "HEAD"]);

    std::fs::write(worktree_path.join("worker-only.txt"), "stray\n").unwrap();
    run_git(&worktree_path, &["add", "worker-only.txt"]);
    run_git(&worktree_path, &["commit", "-m", "worker only"]);
    let stray_head = run_git(&worktree_path, &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "T-contaminated",
            "title": "Contaminated branch task",
            "status": "pending",
            "task_type": "task",
            "assignee": null,
            "merge_target": "epic/test"
        }),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-contaminated",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["merge_target_sync"]["status"], "reset");
    let preserved_ref = json["merge_target_sync"]["preserved_ref"]
        .as_str()
        .expect("preserved ref");
    assert!(preserved_ref.contains("refs/brehon/archive/worker-1/"));
    assert_eq!(
        run_git(&worktree_path, &["rev-parse", "HEAD"]),
        merge_target_head
    );
    assert_eq!(
        run_git(repo_root.path(), &["rev-parse", preserved_ref]),
        stray_head
    );
}

#[tokio::test]
async fn test_assign_workers_moves_changes_requested_task_to_assigned() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task(root.path(), "T-revision", "changes_requested", "task");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-revision",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let task = read_test_task(root.path(), "T-revision");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-2");
}

#[tokio::test]
async fn test_assign_workers_rejects_live_changes_requested_transfer() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-live-revision",
        "changes_requested",
        "task",
        Value::String("worker-1".to_string()),
    );
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    for args in [
        serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-live-revision",
            "worker": "worker-2"
        }),
        serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-live-revision",
            "worker": "worker-2",
            "force_reassign": true
        }),
    ] {
        let result = tool.execute(args).await.unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("already owned by live worker 'worker-1'"),
            "{text}"
        );
        assert!(text.contains("two worker panes"), "{text}");
    }

    let task = read_test_task(root.path(), "T-live-revision");
    assert_eq!(task["status"], "changes_requested");
    assert_eq!(task["assignee"], "worker-1");
}

#[tokio::test]
async fn test_assign_workers_allows_live_changes_requested_same_worker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-same-revision",
        "changes_requested",
        "task",
        Value::String("worker-1".to_string()),
    );
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-same-revision",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let task = read_test_task(root.path(), "T-same-revision");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-1");
}

#[test]
fn test_stage_task_assignment_propagation_clears_stale_same_worker_metadata() {
    let mut task = serde_json::Map::new();
    task.insert(
        "task_id".into(),
        Value::String("T-same-revision".to_string()),
    );
    task.insert(
        "status".into(),
        Value::String("changes_requested".to_string()),
    );
    task.insert("assignee".into(), Value::String("worker-1".to_string()));
    task.insert(
        "assignment_propagation".into(),
        serde_json::json!({
            "owner": "worker-1",
            "assignment_kind": "task",
            "assigned_at": "2026-05-24T01:00:00Z",
            "prompt_id": "prompt-worker-1",
            "delivery_method": "queued",
            "acknowledged_at": "2026-05-24T01:00:05Z",
            "acknowledged_by": "worker-1",
            "acknowledged_via": "task action=mine",
            "progress_started_at": "2026-05-24T01:00:10Z",
            "progress_started_by": "worker-1",
            "progress_started_via": "task action=progress"
        }),
    );

    let propagation = super::tool::stage_task_assignment_propagation(&mut task, "worker-1", "task");

    assert_eq!(task["assignment_propagation"]["owner"], "worker-1");
    assert!(task["assignment_propagation"]["prompt_id"].is_null());
    assert!(task["assignment_propagation"]["acknowledged_at"].is_null());
    assert!(task["assignment_propagation"]["progress_started_at"].is_null());

    let observability = crate::tools::assignment_observability::build_assignment_observability(
        "worker-1",
        "task",
        "T-same-revision",
        None,
        None,
        Some(&propagation),
        false,
    );
    assert_eq!(observability["overall"], "assigned_without_delivery");
}

#[tokio::test]
async fn test_merge_assignment_delivery_metadata_preserves_concurrent_ack_and_progress() {
    let mut task = serde_json::Map::new();
    task.insert(
        "task_id".into(),
        Value::String("T-concurrent-delivery".to_string()),
    );
    task.insert("status".into(), Value::String("assigned".to_string()));
    task.insert("assignee".into(), Value::String("worker-1".to_string()));

    let staged = super::tool::stage_task_assignment_propagation(&mut task, "worker-1", "task");
    task.insert(
        "assignment_propagation".into(),
        serde_json::json!({
            "owner": "worker-1",
            "assignment_kind": "task",
            "assigned_at": staged.assigned_at,
            "prompt_id": null,
            "delivery_method": null,
            "acknowledged_at": "2026-05-24T01:00:05Z",
            "acknowledged_by": "worker-1",
            "acknowledged_via": "task action=mine",
            "progress_started_at": "2026-05-24T01:00:10Z",
            "progress_started_by": "worker-1",
            "progress_started_via": "task action=progress"
        }),
    );

    super::tool::merge_assignment_delivery_metadata(
        &mut task,
        "worker-1",
        "prompt-worker-1",
        "queued",
        Some(&staged),
    );

    assert_eq!(
        task["assignment_propagation"]["prompt_id"],
        Value::String("prompt-worker-1".to_string())
    );
    assert_eq!(
        task["assignment_propagation"]["delivery_method"],
        Value::String("queued".to_string())
    );
    assert_eq!(
        task["assignment_propagation"]["acknowledged_via"],
        Value::String("task action=mine".to_string())
    );
    assert_eq!(
        task["assignment_propagation"]["progress_started_via"],
        Value::String("task action=progress".to_string())
    );
}

#[test]
fn test_persist_assignment_delivery_metadata_writes_prompt_id_before_dispatch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

    let mut task = serde_json::Map::new();
    task.insert(
        "task_id".into(),
        Value::String("T-persist-before-delivery".to_string()),
    );
    task.insert("status".into(), Value::String("assigned".to_string()));
    task.insert("assignee".into(), Value::String("worker-1".to_string()));
    let staged = super::tool::stage_task_assignment_propagation(&mut task, "worker-1", "task");
    assert!(super::paths::write_task("T-persist-before-delivery", &task));

    super::tool::persist_assignment_delivery_metadata(
        "T-persist-before-delivery",
        "worker-1",
        "prompt-before-dispatch",
        crate::tools::agent::PROMPT_QUEUE_DELIVERY_METHOD,
        Some(&staged),
    )
    .expect("persist delivery metadata");

    let stored = read_test_task(root.path(), "T-persist-before-delivery");
    assert_eq!(
        stored["assignment_propagation"]["prompt_id"],
        Value::String("prompt-before-dispatch".to_string())
    );
    assert_eq!(
        stored["assignment_propagation"]["delivery_method"],
        Value::String(crate::tools::agent::PROMPT_QUEUE_DELIVERY_METHOD.to_string())
    );
}

#[test]
fn test_persist_assignment_delivery_metadata_rejects_stale_assignee() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

    let mut task = serde_json::Map::new();
    task.insert(
        "task_id".into(),
        Value::String("T-stale-delivery-owner".to_string()),
    );
    task.insert("status".into(), Value::String("assigned".to_string()));
    task.insert("assignee".into(), Value::String("worker-2".to_string()));
    assert!(super::paths::write_task("T-stale-delivery-owner", &task));

    let err = super::tool::persist_assignment_delivery_metadata(
        "T-stale-delivery-owner",
        "worker-1",
        "prompt-stale-owner",
        crate::tools::agent::PROMPT_QUEUE_DELIVERY_METHOD,
        None,
    )
    .expect_err("stale assignee should fail");

    assert!(
        err.contains("task assignee changed to 'worker-2'"),
        "unexpected error: {err}"
    );
    let stored = read_test_task(root.path(), "T-stale-delivery-owner");
    assert!(stored.get("assignment_propagation").is_none());
}

#[cfg(unix)]
#[test]
fn test_persist_assignment_delivery_metadata_errors_when_task_write_fails() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

    let mut task = serde_json::Map::new();
    task.insert(
        "task_id".into(),
        Value::String("T-delivery-write-failure".to_string()),
    );
    task.insert("status".into(), Value::String("assigned".to_string()));
    task.insert("assignee".into(), Value::String("worker-1".to_string()));
    let staged = super::tool::stage_task_assignment_propagation(&mut task, "worker-1", "task");
    assert!(super::paths::write_task("T-delivery-write-failure", &task));

    let tasks_dir = root.path().join("runtime").join("tasks");
    let original_mode = fs::metadata(&tasks_dir).unwrap().permissions().mode();
    fs::set_permissions(&tasks_dir, fs::Permissions::from_mode(0o555)).unwrap();

    struct RestorePerms<'a> {
        path: &'a std::path::Path,
        mode: u32,
    }
    impl<'a> Drop for RestorePerms<'a> {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.path, fs::Permissions::from_mode(self.mode));
        }
    }
    let _restore = RestorePerms {
        path: &tasks_dir,
        mode: original_mode,
    };

    let err = super::tool::persist_assignment_delivery_metadata(
        "T-delivery-write-failure",
        "worker-1",
        "prompt-write-failure",
        crate::tools::agent::PROMPT_QUEUE_DELIVERY_METHOD,
        Some(&staged),
    )
    .expect_err("read-only tasks dir should fail");

    assert_eq!(err, "write failed for task T-delivery-write-failure");
    let stored = read_test_task(root.path(), "T-delivery-write-failure");
    assert!(stored["assignment_propagation"]["prompt_id"].is_null());
}

#[cfg(unix)]
#[tokio::test]
async fn test_assign_workers_rewrites_propagation_when_delivery_fails() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_SESSION_NAME", "test-session"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    write_test_task(root.path(), "T-delivery-fail", "pending", "task");

    // Block enqueue by placing a file where the queue expects a directory.
    let queue_dir = root.path().join("runtime").join("prompt-queue");
    std::fs::create_dir_all(queue_dir.parent().unwrap()).unwrap();
    std::fs::write(&queue_dir, b"").unwrap();

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-delivery-fail",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["inbox_delivered"], false);

    let task = read_test_task(root.path(), "T-delivery-fail");
    assert_eq!(
        task["assignment_propagation"]["delivery_method"],
        "persisted_not_enqueued"
    );
}

#[test]
fn test_validate_assignment_delivery_entry_rejects_empty_prompt_id() {
    let entry = PromptQueueEntry::new("worker-1", Some("supervisor"), "test").with_prompt_id("");
    let result = super::tool::validate_assignment_delivery_entry(&entry, "T-123", "worker-1");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("assigned_without_delivery"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_validate_assignment_delivery_entry_accepts_valid_prompt_id() {
    let entry = PromptQueueEntry::new("worker-1", Some("supervisor"), "test")
        .with_prompt_id("prompt-abc-123");
    let result = super::tool::validate_assignment_delivery_entry(&entry, "T-123", "worker-1");
    assert_eq!(result.unwrap(), "prompt-abc-123");
}

#[test]
fn test_validate_assignment_delivery_entry_rejects_missing_prompt_id() {
    let mut entry = PromptQueueEntry::new("worker-1", Some("supervisor"), "test");
    entry.prompt_id = None;
    let result = super::tool::validate_assignment_delivery_entry(&entry, "T-123", "worker-1");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("assigned_without_delivery"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_assign_workers_persists_prompt_id_before_delivery() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "supervisor-1"),
    ]);
    write_test_task(root.path(), "T-persisted-prompt", "pending", "task");
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-persisted-prompt",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let prompt_id = payload["prompt_id"]
        .as_str()
        .expect("assign_workers should surface prompt_id");
    assert_eq!(payload["inbox_delivered"], true);

    let stored = read_test_task(root.path(), "T-persisted-prompt");
    assert_eq!(stored["assignment_propagation"]["owner"], "worker-1");
    assert_eq!(
        stored["assignment_propagation"]["prompt_id"],
        Value::String(prompt_id.to_string())
    );
    assert_eq!(
        stored["assignment_propagation"]["delivery_method"],
        Value::String(crate::tools::agent::PROMPT_QUEUE_DELIVERY_METHOD.to_string())
    );
}

#[tokio::test]
async fn test_assign_workers_reseeds_changes_requested_task_from_latest_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo_root = make_git_repo_with_brehon_root();
    let brehon_root = repo_root.path().join(".brehon");
    std::fs::write(
        repo_root.path().join(".git").join("info").join("exclude"),
        ".brehon/worktrees\n",
    )
    .unwrap();

    run_git(repo_root.path(), &["checkout", "-b", "epic/test"]);
    std::fs::write(repo_root.path().join("phase.txt"), "phase base\n").unwrap();
    run_git(repo_root.path(), &["add", "phase.txt"]);
    run_git(repo_root.path(), &["commit", "-m", "phase base"]);
    let merge_target_head = run_git(repo_root.path(), &["rev-parse", "HEAD"]);

    let prior_worker_branch = "brehon/runs/run-1/worker-prior";
    let prior_worker_worktree =
        create_worker_worktree(repo_root.path(), "worker-prior", prior_worker_branch);
    std::fs::create_dir_all(
        prior_worker_worktree
            .join("internal")
            .join("engine")
            .join("gnmi"),
    )
    .unwrap();
    std::fs::write(
        prior_worker_worktree
            .join("internal")
            .join("engine")
            .join("gnmi")
            .join("running_cache.go"),
        "package gnmi\n",
    )
    .unwrap();
    run_git(
        &prior_worker_worktree,
        &["add", "internal/engine/gnmi/running_cache.go"],
    );
    run_git(
        &prior_worker_worktree,
        &["commit", "-m", "prior round implementation"],
    );
    let reviewed_head = run_git(&prior_worker_worktree, &["rev-parse", "HEAD"]);
    assert_ne!(reviewed_head, merge_target_head);

    let rework_worker_branch = "brehon/runs/run-1/worker-2";
    let rework_worker_worktree =
        create_worker_worktree(repo_root.path(), "worker-2", rework_worker_branch);
    assert_eq!(
        run_git(&rework_worker_worktree, &["rev-parse", "HEAD"]),
        merge_target_head
    );

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );
    write_task_json(
        &brehon_root,
        serde_json::json!({
            "task_id": "T-revision-seed",
            "title": "Revision seed task",
            "status": "changes_requested",
            "task_type": "task",
            "assignee": null,
            "merge_target": "epic/test",
            "latest_commit": reviewed_head
        }),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-revision-seed",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["assignment_seed_sync"]["status"], "reset");
    assert_eq!(json["assignment_seed_sync"]["kind"], "latest_commit");
    assert_eq!(json["assignment_seed_sync"]["target_ref"], reviewed_head);
    assert!(json.get("merge_target_sync").is_none());
    assert_eq!(
        run_git(&rework_worker_worktree, &["rev-parse", "HEAD"]),
        reviewed_head
    );
    assert_eq!(
        std::fs::read_to_string(
            rework_worker_worktree
                .join("internal")
                .join("engine")
                .join("gnmi")
                .join("running_cache.go")
        )
        .unwrap(),
        "package gnmi\n"
    );

    let task = read_test_task(&brehon_root, "T-revision-seed");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-2");
}

#[tokio::test]
async fn test_assign_workers_rejects_non_pending_tasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    for status in ["assigned", "in_progress", "in_review", "approved", "merged"] {
        let task_id = format!("T-{status}");
        write_test_task(root.path(), &task_id, status, "task");

        let result = tool
            .execute(serde_json::json!({
                "action": "assign_workers",
                "task_id": task_id,
                "worker": "worker-1"
            }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true), "status={status}");
        let text = extract_text(&result);
        assert!(
            text.contains("status must be 'pending' or 'changes_requested'")
                || text.contains("is terminal"),
            "unexpected error for {status}: {text}"
        );

        let task = read_test_task(root.path(), &format!("T-{status}"));
        assert_eq!(task["status"], status, "status mutated for {status}");
    }
}

#[tokio::test]
async fn test_assign_workers_recovers_orphaned_in_progress_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-orphaned",
        "in_progress",
        "task",
        Value::String("dead-worker".to_string()),
    );
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-orphaned",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["recovered_orphaned_task"], true);
    assert!(json["recovery_note"]
        .as_str()
        .unwrap_or("")
        .contains("dead-worker"));

    let task = read_test_task(root.path(), "T-orphaned");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-2");
    assert!(task["recovery_note"]
        .as_str()
        .unwrap_or("")
        .contains("in_progress"));
}

#[tokio::test]
async fn test_assign_workers_blocks_reassignment_while_review_round_is_unconsolidated() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-active-review",
        "changes_requested",
        "task",
        Value::String("dead-worker".to_string()),
    );
    let task_path = root.path().join("runtime/tasks/T-active-review.json");
    let mut task: Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task["review_owner"] = Value::String("dead-worker".to_string());
    std::fs::write(&task_path, serde_json::to_string_pretty(&task).unwrap()).unwrap();

    let round_dir = root
        .path()
        .join("runtime")
        .join("reviews")
        .join("T-active-review")
        .join("round-1");
    std::fs::create_dir_all(&round_dir).unwrap();
    std::fs::write(
        round_dir.join("request.json"),
        serde_json::json!({
            "task_id": "T-active-review",
            "review_id": "REV-active",
            "requested_by": "supervisor",
            "requested_at": chrono::Utc::now().to_rfc3339(),
            "title": "Active review",
            "description": "Still reviewing",
            "commit": "abc123",
            "base_commit": "base123",
            "merge_target_head": "base123",
            "commits": ["abc123"],
            "context": ""
        })
        .to_string(),
    )
    .unwrap();

    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-active-review",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("active or unconsolidated review round"),
        "{}",
        extract_text(&result)
    );

    let task = read_test_task(root.path(), "T-active-review");
    assert_eq!(task["assignee"], "dead-worker");
    assert_eq!(task["review_owner"], "dead-worker");
}

#[tokio::test]
async fn test_assign_workers_recovers_startup_normalized_orphaned_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task(root.path(), "T-orphaned-pending", "pending", "task");
    let task_path = root.path().join("runtime/tasks/T-orphaned-pending.json");
    let mut task: Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task["orphaned_assignee"] = Value::String("dead-worker".to_string());
    task["orphaned_status"] = Value::String("in_progress".to_string());
    std::fs::write(&task_path, serde_json::to_string_pretty(&task).unwrap()).unwrap();

    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-orphaned-pending",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["recovered_orphaned_task"], true);

    let task = read_test_task(root.path(), "T-orphaned-pending");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-2");
    assert!(task.get("orphaned_assignee").is_none());
    assert!(task.get("orphaned_status").is_none());
    assert!(task["recovery_note"]
        .as_str()
        .unwrap_or("")
        .contains("dead-worker"));
}

#[tokio::test]
async fn test_assign_workers_rejects_live_in_progress_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-live-progress",
        "in_progress",
        "task",
        Value::String("worker-1".to_string()),
    );
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-live-progress",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("status must be 'pending' or 'changes_requested'"));

    let task = read_test_task(root.path(), "T-live-progress");
    assert_eq!(task["status"], "in_progress");
    assert_eq!(task["assignee"], "worker-1");
}

#[tokio::test]
async fn test_assign_workers_rejects_worker_with_other_active_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-in-progress",
        "in_progress",
        "task",
        Value::String("worker-1".to_string()),
    );
    write_test_task(root.path(), "T-new", "pending", "task");
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-new",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("already has active task(s)"));
    assert!(text.contains("T-in-progress [in_progress]"));

    let new_task = read_test_task(root.path(), "T-new");
    assert_eq!(new_task["status"], "pending");
    assert!(new_task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_keeps_completed_assigned_task_reserved() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-complete",
        "assigned",
        "task",
        Value::String("worker-1".to_string()),
    );
    let complete_path = root
        .path()
        .join("runtime")
        .join("tasks")
        .join("T-complete.json");
    let mut complete: Value =
        serde_json::from_str(&std::fs::read_to_string(&complete_path).unwrap()).unwrap();
    complete["percent"] = serde_json::json!(100);
    std::fs::write(
        &complete_path,
        serde_json::to_string_pretty(&complete).unwrap(),
    )
    .unwrap();

    write_test_task(root.path(), "T-new", "pending", "task");
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-new",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("already has active task(s)"));
    assert!(text.contains("T-complete [assigned]"));
    let new_task = read_test_task(root.path(), "T-new");
    assert_eq!(new_task["status"], "pending");
    assert!(new_task["assignee"].is_null());
}

#[tokio::test]
async fn test_worker_status_ignores_supervisor_conflict_but_keeps_completed_revision_tasks_busy() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);

    write_test_task_with_assignee(
        root.path(),
        "T-complete-revision",
        "changes_requested",
        "task",
        Value::String("worker-1".to_string()),
    );
    let complete_path = root
        .path()
        .join("runtime")
        .join("tasks")
        .join("T-complete-revision.json");
    let mut complete: Value =
        serde_json::from_str(&std::fs::read_to_string(&complete_path).unwrap()).unwrap();
    complete["percent"] = serde_json::json!(100);
    std::fs::write(
        &complete_path,
        serde_json::to_string_pretty(&complete).unwrap(),
    )
    .unwrap();

    write_test_task_with_assignee(
        root.path(),
        "T-conflict",
        "changes_requested",
        "task",
        Value::String("worker-2".to_string()),
    );
    let conflict_path = root
        .path()
        .join("runtime")
        .join("tasks")
        .join("T-conflict.json");
    let mut conflict: Value =
        serde_json::from_str(&std::fs::read_to_string(&conflict_path).unwrap()).unwrap();
    conflict["integration_conflict"] = serde_json::json!({
        "owner": "supervisor",
        "source": "approved_integration"
    });
    std::fs::write(
        &conflict_path,
        serde_json::to_string_pretty(&conflict).unwrap(),
    )
    .unwrap();

    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "worker_status" }))
        .await
        .unwrap();
    assert!(result.is_error.is_none());
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["busy_worker_count"], 1);
    assert_eq!(payload["idle_worker_count"], 0);

    let workers = payload["workers"].as_array().unwrap();
    let revision_worker = workers
        .iter()
        .find(|worker| worker["name"] == "worker-1")
        .expect("worker-1 missing");
    let availability = revision_worker["availability"].as_str().unwrap();
    assert!(
        availability.contains("busy"),
        "expected busy availability, got: {availability}"
    );
    assert!(
        availability.contains("T-complete-revision"),
        "expected task id in availability, got: {availability}"
    );
    assert!(
        availability.contains("changes_requested"),
        "expected changes_requested in availability, got: {availability}"
    );

    let conflict_worker = workers
        .iter()
        .find(|worker| worker["name"] == "worker-2")
        .expect("worker-2 missing");
    assert_eq!(
        conflict_worker["availability"],
        "unavailable (missing worktree)"
    );
}

#[tokio::test]
async fn test_worker_status_does_not_count_pending_or_blocked_tasks_as_busy() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);

    write_test_task_with_assignee(
        root.path(),
        "T-pending",
        "pending",
        "task",
        Value::String("worker-1".to_string()),
    );
    write_test_task_with_assignee(
        root.path(),
        "T-blocked",
        "blocked",
        "task",
        Value::String("worker-2".to_string()),
    );

    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    crate::tools::agent::write_session_file(
        "worker-2",
        "worker",
        "worker-2-session",
        Some("opencode"),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "worker_status" }))
        .await
        .unwrap();
    assert!(result.is_error.is_none());
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["busy_worker_count"], 0, "{payload}");
    assert_eq!(payload["idle_worker_count"], 0, "{payload}");
}

#[tokio::test]
async fn test_assign_workers_rejects_worker_with_review_held_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-under-review",
        "in_review",
        "task",
        Value::String("worker-1".to_string()),
    );
    write_test_task(root.path(), "T-new", "pending", "task");
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-new",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("already has active task(s)"));
    assert!(text.contains("T-under-review [in_review]"));
    let new_task = read_test_task(root.path(), "T-new");
    assert_eq!(new_task["status"], "pending");
    assert!(new_task["assignee"].is_null());
}

#[tokio::test]
async fn test_assign_workers_rejects_control_plane_task_scope() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task(root.path(), "T-control-plane", "pending", "task");
    let mut task = read_test_task(root.path(), "T-control-plane");
    task["file_hints"] = serde_json::json!([".brehon/config.yaml"]);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-control-plane.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-control-plane",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("targets live Brehon control-plane state"));
}

#[tokio::test]
async fn test_assign_workers_rejects_epics() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task(root.path(), "EPIC-1", "pending", "epic");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file(
        "worker-1",
        "worker",
        "worker-1-session",
        Some("opencode"),
    );
    let tool = FactoryTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "EPIC-1",
            "worker": "worker-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("Cannot assign epic"));
}

#[tokio::test]
async fn test_set_ownership_requires_supervisor_and_rejects_terminal_tasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    write_test_task(root.path(), "T-terminal", "merged", "task");

    let _worker_env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = FactoryTool::new();
    let worker_result = tool
        .execute(serde_json::json!({
            "action": "set_ownership",
            "task_id": "T-terminal",
            "worker": "worker-2"
        }))
        .await
        .unwrap();
    assert_eq!(worker_result.is_error, Some(true));
    assert!(extract_text(&worker_result).contains("Only supervisors can assign workers"));
    drop(_worker_env);

    let _supervisor_env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let supervisor_result = tool
        .execute(serde_json::json!({
            "action": "set_ownership",
            "task_id": "T-terminal",
            "worker": "worker-2"
        }))
        .await
        .unwrap();
    assert_eq!(supervisor_result.is_error, Some(true));
    assert!(extract_text(&supervisor_result).contains("is terminal"));
}

#[tokio::test]
async fn test_set_ownership_replaces_stale_assignment_propagation() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = FactoryTool::new();
    write_test_task_with_assignee(
        root.path(),
        "T-reassign",
        "assigned",
        "task",
        Value::String("worker-1".to_string()),
    );
    let mut task = read_test_task(root.path(), "T-reassign");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-worker-1",
        "delivery_method": "queued",
        "acknowledged_at": "2026-05-24T01:00:05Z",
        "acknowledged_by": "worker-1",
        "acknowledged_via": "task action=mine",
        "progress_started_at": "2026-05-24T01:00:10Z",
        "progress_started_by": "worker-1",
        "progress_started_via": "task action=progress"
    });
    write_test_task_json(root.path(), "T-reassign", &task);

    let result = tool
        .execute(serde_json::json!({
            "action": "set_ownership",
            "task_id": "T-reassign",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let stored = read_test_task(root.path(), "T-reassign");
    assert_eq!(stored["assignee"], "worker-2");
    assert_eq!(stored["assignment_propagation"]["owner"], "worker-2");
    assert!(stored["assignment_propagation"]["prompt_id"].is_null());
    assert!(stored["assignment_propagation"]["acknowledged_at"].is_null());
    assert!(stored["assignment_propagation"]["progress_started_at"].is_null());
}

#[tokio::test]
async fn test_worker_status_shows_task_status() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    write_test_task_with_assignee(
        root.path(),
        "T-in-progress",
        "in_progress",
        "task",
        Value::String("worker-with-task".to_string()),
    );
    write_test_task_with_assignee(
        root.path(),
        "T-in-review",
        "in_review",
        "task",
        Value::String("worker-in-review".to_string()),
    );
    crate::tools::agent::write_session_file(
        "worker-with-task",
        "worker",
        "worker-session-1",
        Some("opencode"),
    );
    crate::tools::agent::write_session_file(
        "worker-in-review",
        "worker",
        "worker-session-2",
        Some("opencode"),
    );
    crate::tools::agent::write_session_file(
        "idle-worker",
        "worker",
        "worker-session-3",
        Some("opencode"),
    );

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "worker_status" }))
        .await
        .unwrap();
    assert!(result.is_error.is_none());
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();

    let workers = v["workers"].as_array().unwrap();

    let in_progress_worker = workers
        .iter()
        .find(|w| w["name"] == "worker-with-task")
        .expect("worker-with-task missing");
    let availability = in_progress_worker["availability"].as_str().unwrap();
    assert!(
        availability.contains("busy"),
        "expected 'busy' in availability, got: {}",
        availability
    );
    assert!(
        availability.contains("T-in-progress"),
        "expected task ID in availability, got: {}",
        availability
    );
    assert!(
        availability.contains("in_progress"),
        "expected task status in availability, got: {}",
        availability
    );

    let review_worker = workers
        .iter()
        .find(|w| w["name"] == "worker-in-review")
        .expect("worker-in-review missing");
    let review_avail = review_worker["availability"].as_str().unwrap();
    assert!(
        review_avail.contains("busy"),
        "expected 'busy' in availability, got: {}",
        review_avail
    );
    assert!(
        review_avail.contains("T-in-review"),
        "expected task ID in availability, got: {}",
        review_avail
    );
    assert!(
        review_avail.contains("in_review"),
        "expected task status in availability, got: {}",
        review_avail
    );

    let idle_worker = workers
        .iter()
        .find(|w| w["name"] == "idle-worker")
        .expect("idle-worker missing");
    assert_eq!(
        idle_worker["availability"],
        "unavailable (missing worktree)"
    );
}

#[tokio::test]
async fn test_worker_status_health_fields() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);

    // Create a worker session with a recent last_seen_at
    crate::tools::agent::write_session_file(
        "health-worker",
        "worker",
        "health-session",
        Some("opencode"),
    );

    // Create an active task for the worker with updated_at
    let tasks_dir = root.path().join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let now = chrono::Utc::now();
    let task = serde_json::json!({
        "task_id": "T-health",
        "title": "Health task",
        "status": "in_progress",
        "task_type": "task",
        "assignee": "health-worker",
        "updated_at": now.to_rfc3339()
    });
    std::fs::write(
        tasks_dir.join("T-health.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = FactoryTool::new();
    let args = serde_json::json!({ "action": "worker_status" });
    let result = tool.execute(args).await.unwrap();
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let workers = v["workers"].as_array().unwrap();
    let worker = workers
        .iter()
        .find(|w| w["name"] == "health-worker")
        .expect("health-worker missing");

    // heartbeat_live should be true (session was just created)
    assert_eq!(worker["heartbeat_live"], true);
    // output_live should be true (task was just updated)
    assert_eq!(worker["output_live"], true);
    // last_event_at should be present
    assert!(worker["last_event_at"].is_string());
    // idle_duration_secs should be a small number
    assert!(worker["idle_duration_secs"].as_i64().unwrap() < 60);
    // nudge section should exist
    assert!(worker["nudge"].is_object());
    assert_eq!(worker["nudge"]["nudges_sent_count"], 0);
    // worktree section should exist
    assert!(worker["worktree"].is_object() || worker["worktree"].is_null());
}

#[tokio::test]
async fn test_unavailable_worker_is_not_live_and_status_marks_orphaned_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);

    write_test_task_with_assignee(
        root.path(),
        "T-unavailable",
        "in_progress",
        "task",
        Value::String("native-worker".to_string()),
    );
    crate::tools::agent::write_session_file(
        "native-worker",
        "worker",
        "native-session",
        Some("native-agent"),
    );
    let health_dir = root.path().join("runtime").join("agent-health");
    std::fs::create_dir_all(&health_dir).unwrap();
    std::fs::write(
        health_dir.join("native-worker.json"),
        serde_json::json!({
            "agent": "native-worker",
            "role": "worker",
            "status": "unavailable",
            "reason": "stream_idle_timeout"
        })
        .to_string(),
    )
    .unwrap();

    let live = super::workers::live_worker_names();
    assert!(!live.contains("native-worker"));

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "worker_status" }))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let workers = v["workers"].as_array().unwrap();
    let worker = workers
        .iter()
        .find(|w| w["name"] == "native-worker")
        .expect("native-worker missing");
    assert_eq!(worker["agent_health"]["status"], "unavailable");
    assert_eq!(worker["available_for_assignment"], false);
    assert!(worker["availability"]
        .as_str()
        .unwrap()
        .contains("orphaned active task T-unavailable"));
}

#[tokio::test]
async fn test_worker_status_finds_run_scoped_worktree() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);

    crate::tools::agent::write_session_file(
        "worker-runscoped",
        "worker",
        "worker-runscoped-session",
        Some("codex"),
    );

    let worktree_path = root
        .path()
        .join("worktrees")
        .join("runs")
        .join("run-1")
        .join("worker-runscoped");
    std::fs::create_dir_all(&worktree_path).unwrap();
    git2::Repository::init(&worktree_path).unwrap();

    let tool = FactoryTool::new();
    let result = tool
        .execute(serde_json::json!({ "action": "worker_status" }))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let workers = v["workers"].as_array().unwrap();
    let worker = workers
        .iter()
        .find(|w| w["name"] == "worker-runscoped")
        .expect("worker-runscoped missing");
    assert_eq!(worker["worktree"]["worktree_exists"], true);
    assert_eq!(
        worker["worktree"]["worktree_path"].as_str().unwrap(),
        worktree_path.to_string_lossy()
    );
}
