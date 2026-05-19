use super::TaskActionsTool;
use crate::server::{ContentBlock, ToolResult};
use crate::tools::{Tool, TEST_ENV_LOCK};
use brehon_ports::{EventStore, ProofStore};
use brehon_store_fjall::FjallEventStore;
use brehon_types::TaskId;
use serde_json::Value;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

struct ScopedEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut all_vars = vars.to_vec();
        for key in [
            "BREHON_WORKTREE_BRANCH",
            "BREHON_WORKSPACE_ROOT",
            "BREHON_PROJECT_ROOT",
        ] {
            if !all_vars.iter().any(|(existing, _)| existing == &key) {
                all_vars.push((key, ""));
            }
        }
        let mut saved = Vec::new();
        for (key, value) in &all_vars {
            saved.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { saved }
    }
}

impl Drop for ScopedEnv {
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

fn extract_text(result: &ToolResult) -> String {
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

fn write_test_task(root: &Path, task_id: &str, status: &str) {
    let tasks_dir = root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let task = serde_json::json!({
        "task_id": task_id,
        "title": format!("Task {task_id}"),
        "description": "proof fixture",
        "status": status,
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "percent": 0
    });
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
}

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
    run_git(workspace, &["init", "-b", "main"]);
    run_git(workspace, &["config", "user.email", "test@example.com"]);
    run_git(workspace, &["config", "user.name", "Test User"]);
    std::fs::write(workspace.join("README.md"), "seed\n").unwrap();
    run_git(workspace, &["add", "README.md"]);
    run_git(workspace, &["commit", "-m", "seed"]);
    run_git(workspace, &["checkout", "-b", "worker/test"]);
    run_git(workspace, &["rev-parse", "HEAD"])
}

fn proof_tool(root: &TempDir) -> (TaskActionsTool, Arc<dyn ProofStore + Send + Sync>) {
    let store = Arc::new(FjallEventStore::new(root.path().join("fjall")).unwrap());
    let event_store: Arc<dyn EventStore + Send + Sync> = store.clone();
    let proof_store: Arc<dyn ProofStore + Send + Sync> = store.clone();
    (
        TaskActionsTool::new()
            .with_event_store(event_store)
            .with_proof_store(proof_store.clone()),
        proof_store,
    )
}

#[tokio::test]
async fn proof_completion_creates_worker_evidence_bundle() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "completed\n").unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    write_test_task(root.path(), "T-proof-complete", "in_progress");
    let (tool, proof_store) = proof_tool(&root);

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-proof-complete",
            "notes": "Implemented feature; cargo test -p brehon-mcp proof passed."
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let commit = json["latest_commit"].as_str().unwrap();
    assert_eq!(json["proof_status"], "recorded");

    let bundle = proof_store
        .proof_bundle_for_task(&TaskId::new("T-proof-complete"))
        .await
        .unwrap()
        .expect("proof bundle should exist");
    assert!(bundle
        .commands
        .iter()
        .any(|command| command.command.contains("action=checkpoint")));
    assert!(bundle
        .commands
        .iter()
        .any(|command| command.command.contains("action=progress")));
    assert!(bundle.commits.contains(&commit.to_string()));
    assert!(bundle
        .diff_summary
        .as_deref()
        .unwrap_or("")
        .contains("feature.txt"));
    assert!(bundle.test_results.iter().any(|check| check
        .command
        .as_deref()
        .unwrap_or("")
        .contains("cargo test")));
    assert!(!bundle
        .blockers
        .iter()
        .any(|blocker| { blocker.blocker_id.as_deref() == Some("missing-worker-test-evidence") }));
}

#[tokio::test]
async fn proof_worker_update_records_blocker_evidence() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    write_test_task(root.path(), "T-proof-update", "in_progress");
    let (tool, proof_store) = proof_tool(&root);

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-proof-update",
            "blockers": "waiting on a fixture refresh"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["proof_status"], "blocked");

    let bundle = proof_store
        .proof_bundle_for_task(&TaskId::new("T-proof-update"))
        .await
        .unwrap()
        .expect("proof bundle should exist");
    assert!(bundle
        .commands
        .iter()
        .any(|command| command.command.contains("action=update")));
    assert!(bundle
        .blockers
        .iter()
        .any(|blocker| blocker.summary.contains("fixture refresh")));
}

#[tokio::test]
async fn proof_missing_store_is_visible_on_worker_update() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    write_test_task(root.path(), "T-proof-missing", "in_progress");
    let tool = TaskActionsTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-proof-missing",
            "notes": "worker note"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["proof_status"], "unavailable");
    assert!(json["proof_warning"]
        .as_str()
        .unwrap_or("")
        .contains("worker proof evidence was not recorded"));
}
