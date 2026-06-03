use super::*;
use crate::server::ContentBlock;
use crate::tools::test_support::{
    write_pane_assignment_context_fixture, write_prompt_delivery_fixture,
};
use crate::tools::{ScopedEnv, TEST_ENV_LOCK};
use brehon_types::TaskCompletionMode;
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestCaseError, TestRunner};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tracing_subscriber::fmt::MakeWriter;

// Items from sibling submodules needed by tests
use super::super::epic::{
    apply_supervisor_integration_conflict, check_epic_completion,
    clear_task_supervisor_integration_conflict, default_conflict_owner,
    ensure_epic_integration_worktree, read_current_review_request,
    INTEGRATION_CONFLICT_BLOCKER_PREFIX,
};
use super::super::git_ops::{
    cherry_pick_in_progress_in, cherry_pick_sha_in, commit_workspace_checkpoint,
    current_workspace_root, detect_default_branch, detect_remote_merge_status,
    ensure_checkpoint_cwd_is_isolated, git_patch_id, is_patch_equivalent_in_window_in,
    tree_matches_after, unmerged_files, MergeStatus,
};
use super::super::lifecycle::{is_container_task, validate_status_transition};
use super::super::paths::{project_root, workspace_root};
use crate::tools::task_actions::update_task_status_atomic;
use crate::tools::verification::reviewed_commits;
use std::path::PathBuf;

#[derive(Clone, Default)]
struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

impl SharedLogBuffer {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl<'a> MakeWriter<'a> for SharedLogBuffer {
    type Writer = SharedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedLogWriter(self.0.clone())
    }
}

impl Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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

fn write_test_task(root: &Path, task_id: &str, status: &str, task_type: &str) {
    let default_mode = if is_container_task(task_type) {
        "close"
    } else {
        "merge"
    };
    write_test_task_with_mode(
        root,
        task_id,
        status,
        task_type,
        default_mode,
        "test fixture",
    );
}

fn write_test_task_with_mode(
    root: &Path,
    task_id: &str,
    status: &str,
    task_type: &str,
    completion_mode: &str,
    description: &str,
) {
    let tasks_dir = root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let task = serde_json::json!({
        "task_id": task_id,
        "title": format!("Task {task_id}"),
        "description": description,
        "status": status,
        "task_type": task_type,
        "completion_mode": completion_mode,
        "assignee": "worker-1",
        "percent": 0
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

fn count_log_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn read_worker_recycle_requests(root: &Path) -> Vec<Value> {
    let dir = root.join("runtime").join("worker-recycle-queue");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
        })
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|content| serde_json::from_str::<Value>(&content).ok())
        .map(|value| {
            value
                .get("entry")
                .cloned()
                .filter(|entry| entry.is_object())
                .unwrap_or(value)
        })
        .collect()
}

/// Collect every queued prompt payload under `<root>/runtime/prompt-queue/`.
///
/// The queue stores envelopes shaped like:
/// `{ "session_name": "...", "entry": { "target": "...", ... } }`.
/// This helper unwraps `entry` so existing assertions can read top-level
/// `target`/`message` fields directly, and injects the envelope session name as
/// `payload["session_name"]`.
fn read_queued_prompts(root: &Path) -> Vec<Value> {
    fn walk(dir: &Path, out: &mut Vec<Value>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(&path, out);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            // Skip tmp files written atomically via rename().
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(payload) = serde_json::from_str::<Value>(&content) {
                if let Some(mut entry_payload) = payload.get("entry").cloned() {
                    if let Some(session_name) = payload.get("session_name").cloned() {
                        if let Some(entry_obj) = entry_payload.as_object_mut() {
                            entry_obj.insert("session_name".to_string(), session_name);
                        }
                    }
                    out.push(entry_payload);
                } else {
                    out.push(payload);
                }
            }
        }
    }
    let mut payloads = Vec::new();
    walk(&root.join("runtime").join("prompt-queue"), &mut payloads);
    payloads
}

fn make_test_root() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn task_tool_startup_migrates_legacy_integration_conflicts_and_logs_once() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    for (task_id, conflict_file) in [
        ("T-migrate-1", "src/conflict-a.txt"),
        ("T-migrate-2", "src/conflict-b.txt"),
        ("T-migrate-3", "src/conflict-c.txt"),
    ] {
        let task = serde_json::json!({
            "task_id": task_id,
            "title": format!("Legacy {task_id}"),
            "status": "changes_requested",
            "task_type": "task",
            "completion_mode": "merge",
            "merge_target": "epic/test-feature",
            "updated_at": "2026-04-23T00:00:00Z",
            "integration_conflict": {
                "owner": "supervisor",
                "merge_target": "epic/test-feature",
                "reviewed_commits": [format!("{task_id}-commit")],
                "conflicting_files": [conflict_file],
                "recorded_at": "2026-04-23T00:00:00Z"
            }
        });
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
    }

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let logs = SharedLogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .with_target(false)
        .compact()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let _tool = TaskActionsTool::new();
    let first_logs = logs.contents();

    for task_id in ["T-migrate-1", "T-migrate-2", "T-migrate-3"] {
        let task = read_test_task(&brehon_root, task_id);
        assert_eq!(task["integration"]["phase"], "cherry_picking");
        assert_eq!(task["integration"]["epic_branch"], "epic/test-feature");
        assert!(
            task.get("integration_conflict").is_some(),
            "legacy compatibility blob should be preserved"
        );
        assert!(
            first_logs.contains(task_id),
            "migration log should mention {task_id}: {first_logs}"
        );
    }
    assert_eq!(
        count_log_occurrences(&first_logs, "migrated integration_conflict → integration"),
        3,
        "expected one migration log per legacy task: {first_logs}"
    );

    let _tool_again = TaskActionsTool::new();
    let second_logs = logs.contents();
    assert_eq!(
        count_log_occurrences(&second_logs, "migrated integration_conflict → integration"),
        3,
        "second startup should be a no-op: {second_logs}"
    );
}

#[test]
fn task_tool_startup_warns_when_migrated_task_write_fails() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let task_id = "T-migrate-fail";
    let task = serde_json::json!({
        "task_id": task_id,
        "title": "Legacy migration write failure",
        "status": "changes_requested",
        "task_type": "task",
        "completion_mode": "merge",
        "merge_target": "epic/test-feature",
        "updated_at": "2026-04-23T00:00:00Z",
        "integration_conflict": {
            "owner": "supervisor",
            "merge_target": "epic/test-feature",
            "reviewed_commits": ["abc123"],
            "conflicting_files": ["src/conflict-a.txt"],
            "recorded_at": "2026-04-23T00:00:00Z"
        }
    });
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    std::fs::create_dir(tasks_dir.join(format!(".{task_id}.tmp"))).unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let logs = SharedLogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_target(false)
        .compact()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let _tool = TaskActionsTool::new();
    let task = read_test_task(&brehon_root, task_id);
    let log_output = logs.contents();

    assert!(
        task.get("integration").is_none(),
        "failed write should leave task unmigrated on disk"
    );
    assert!(
        task.get("integration_conflict").is_some(),
        "legacy blob should remain present after failed write"
    );
    assert!(
        log_output.contains(task_id),
        "warning log should mention {task_id}: {log_output}"
    );
    assert!(
        log_output.contains("failed to persist migrated task"),
        "expected persistence warning in logs: {log_output}"
    );
}

#[test]
fn task_tool_startup_migration_matches_production_fixture_snapshot() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("T-legacy-phase-3.json"),
        include_str!("fixtures/phase3_legacy_midflight_task.json"),
    )
    .unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);

    let _tool = TaskActionsTool::new();
    let migrated = std::fs::read_to_string(tasks_dir.join("T-legacy-phase-3.json")).unwrap();
    assert_eq!(
        migrated.trim(),
        include_str!("fixtures/phase3_legacy_midflight_task.migrated.json").trim()
    );

    let _tool_again = TaskActionsTool::new();
    let migrated_again = std::fs::read_to_string(tasks_dir.join("T-legacy-phase-3.json")).unwrap();
    assert_eq!(migrated_again, migrated, "second startup should be a no-op");
}

#[tokio::test]
async fn test_integrate_action_resumes_and_completes_after_startup_migrates_legacy_conflict_blob() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    run_git(workspace.path(), &["checkout", "main"]);

    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-legacy-resume"],
    );
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker branch change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let bootstrap_tool = TaskActionsTool::new();

    let epic_json =
        create_epic_for_test(&bootstrap_tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let integration_worktree = epic_json["integration_worktree"]
        .as_str()
        .unwrap()
        .to_string();
    let subtask_json =
        create_subtask_for_test(&bootstrap_tool, "Legacy Resume Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let cherry_pick_error = run_git_expect_failure(
        Path::new(&integration_worktree),
        &["cherry-pick", "-x", &reviewed_commit],
    );
    assert!(
        cherry_pick_error.contains("conflict") || cherry_pick_error.contains("Merge conflict"),
        "unexpected cherry-pick failure: {cherry_pick_error}"
    );
    let cherry_pick_head = git_path_in(Path::new(&integration_worktree), "CHERRY_PICK_HEAD");
    assert!(
        cherry_pick_head.exists(),
        "expected CHERRY_PICK_HEAD to exist for the legacy in-flight task"
    );

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["integration_status"] = "pending".into();
    let task_object = task.as_object_mut().unwrap();
    apply_supervisor_integration_conflict(
        task_object,
        "changes_requested",
        "epic/test-feature",
        &reviewed_commit,
        std::slice::from_ref(&reviewed_commit),
        &[String::from("src/conflict.txt")],
        "approved_integration",
        Some("worker-1"),
    );
    task_object.remove("integration");
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&reviewed_commit],
    );

    drop(bootstrap_tool);
    let migrated_tool = TaskActionsTool::new();

    let migrated = read_test_task(&brehon_root, subtask_id);
    assert_eq!(migrated["status"], "changes_requested");
    assert_eq!(migrated["integration"]["phase"], "cherry_picking");
    assert_eq!(
        migrated["integration"]["cherry_pick_base_head"],
        run_git(Path::new(&integration_worktree), &["rev-parse", "HEAD"])
    );
    assert_eq!(
        migrated["integration"]["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );
    assert_eq!(
        migrated["integration"]["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    assert!(
        migrated.get("integration_conflict").is_some(),
        "migration should preserve the compatibility blob"
    );

    std::fs::write(
        Path::new(&integration_worktree).join("src/conflict.txt"),
        "epic branch\nworker branch\n",
    )
    .unwrap();
    run_git(
        Path::new(&integration_worktree),
        &["add", "src/conflict.txt"],
    );
    run_git(
        Path::new(&integration_worktree),
        &["cherry-pick", "--continue"],
    );

    let resumed = migrated_tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(resumed.is_error.is_none(), "{}", extract_text(&resumed));
    let resumed_json: Value = serde_json::from_str(&extract_text(&resumed)).unwrap();
    assert_eq!(resumed_json["action"], "integrated");
    assert_eq!(resumed_json["integration_phase"], "complete");
    assert_eq!(resumed_json["status"], "integrated");
    assert_eq!(resumed_json["terminal_status"], "closed");
    assert_eq!(resumed_json["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(resumed_json["next_action_for_brehon"]["kind"], "none");
    assert_eq!(resumed_json["reviewed_commit"], reviewed_commit);
    assert_eq!(
        resumed_json["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
    assert_eq!(
        run_git(
            Path::new(&integration_worktree),
            &["show", "HEAD:src/conflict.txt"],
        ),
        "epic branch\nworker branch"
    );
    assert!(
        !cherry_pick_head.exists(),
        "resume-and-complete should clear CHERRY_PICK_HEAD"
    );
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

fn run_git_expect_failure(workspace: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        !output.status.success(),
        "expected git {} to fail but it succeeded",
        args.join(" ")
    );
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        stderr
    } else {
        stdout
    }
}

fn git_path_in(workspace: &Path, logical_path: &str) -> PathBuf {
    let path = run_git(workspace, &["rev-parse", "--git-path", logical_path]);
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    }
}

fn init_git_workspace(workspace: &Path) -> String {
    run_git(workspace, &["init", "-b", "main"]);
    run_git(workspace, &["config", "user.email", "test@example.com"]);
    run_git(workspace, &["config", "user.name", "Test User"]);
    std::fs::write(workspace.join("README.md"), "seed\n").unwrap();
    run_git(workspace, &["add", "README.md"]);
    run_git(workspace, &["commit", "-m", "seed"]);
    // Leave HEAD on a worker-style branch so worker actions
    // (`progress=100`, `checkpoint`, `complete`) are not blocked by the
    // "refuse to mutate the default branch" guards in
    // `commit_workspace_checkpoint` / `ensure_worker_branch_safe_for_task`.
    // This mirrors how real worker worktrees are provisioned: a fresh
    // branch off main, never `main` itself.
    run_git(workspace, &["checkout", "-b", "worker/test"]);
    run_git(workspace, &["rev-parse", "HEAD"])
}

#[test]
fn tree_matches_after_rejects_same_path_with_different_blob_content() {
    let workspace = TempDir::new().unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/reviewed"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/file.txt"), "reviewed\n").unwrap();
    run_git(workspace.path(), &["add", "src/file.txt"]);
    run_git(workspace.path(), &["commit", "-m", "reviewed change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["checkout", "-b", "epic/test"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/file.txt"), "different\n").unwrap();
    run_git(workspace.path(), &["add", "src/file.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "same path different content"],
    );

    assert!(
        !tree_matches_after(workspace.path(), &reviewed_commit, "epic/test").unwrap(),
        "path-only overlap must not count as reviewed content"
    );

    std::fs::write(workspace.path().join("src/file.txt"), "reviewed\n").unwrap();
    run_git(workspace.path(), &["add", "src/file.txt"]);
    run_git(workspace.path(), &["commit", "-m", "same reviewed content"]);

    assert!(
        tree_matches_after(workspace.path(), &reviewed_commit, "epic/test").unwrap(),
        "matching blob content should prove the reviewed tree is present"
    );
}

fn ensure_test_project_repo() {
    let Some(root) = project_root() else {
        return;
    };
    if root.join(".git").exists() {
        return;
    }
    init_git_workspace(&root);
}

#[test]
fn project_root_ignores_blank_workspace_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", "   "),
    ]);

    assert_eq!(project_root().as_deref(), Some(root.path()));
}

#[test]
fn project_root_ignores_blank_project_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", "   "),
    ]);

    assert_eq!(project_root().as_deref(), Some(root.path()));
}

#[test]
fn project_root_trims_project_root_value() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let wrapped = format!("  {}  ", root.path().display());
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_PROJECT_ROOT", &wrapped)]);

    assert_eq!(project_root().as_deref(), Some(root.path()));
}

#[test]
fn project_root_trims_workspace_root_value() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let brehon_root = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    std::fs::create_dir(workspace.path().join(".git")).unwrap();
    let wrapped = format!("  {}  ", workspace.path().display());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", &wrapped),
    ]);

    assert_eq!(project_root().as_deref(), Some(workspace.path()));
}

#[test]
fn workspace_root_ignores_blank_workspace_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = TempDir::new().unwrap();
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    assert_eq!(workspace_root().as_deref(), Some(temp.path()));
}

#[test]
fn workspace_root_trims_workspace_root_value() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = TempDir::new().unwrap();
    let wrapped = format!("  {}  ", workspace.path().display());
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_WORKSPACE_ROOT", &wrapped)]);

    assert_eq!(workspace_root().as_deref(), Some(workspace.path()));
}

#[test]
fn current_workspace_root_rejects_blank_workspace_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_WORKSPACE_ROOT", "   ")]);

    assert_eq!(
        current_workspace_root().unwrap_err(),
        "No BREHON_WORKSPACE_ROOT available. This action must run from a worker worktree."
    );
}

#[test]
fn checkpoint_cwd_guard_rejects_primary_project_checkout() {
    // Regression: a worker `task action=checkpoint` ever pointed at the
    // shared repo root (via mis-set BREHON_WORKSPACE_ROOT) silently ran
    // `git add -A; git commit` against `main`, and on commit failure left
    // the shared index dirty with worker-branch content. Guard refuses to
    // run when cwd canonicalizes to the project root.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = TempDir::new().unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_PROJECT_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    let err = ensure_checkpoint_cwd_is_isolated(workspace.path()).unwrap_err();
    assert!(
        err.contains("primary project checkout"),
        "expected project-root rejection, got: {err}"
    );

    let commit_err = commit_workspace_checkpoint(workspace.path(), "wip").unwrap_err();
    assert!(
        commit_err.contains("primary project checkout"),
        "commit_workspace_checkpoint must inherit the cwd guard, got: {commit_err}"
    );
}

#[test]
fn checkpoint_cwd_guard_rejects_default_branch() {
    // Even if cwd is not the project root, refuse when HEAD is on the
    // project's default branch. Workers always belong on a dedicated
    // worker branch.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = TempDir::new().unwrap();
    init_git_workspace(workspace.path());
    // Force HEAD back onto main so the guard's branch check fires.
    run_git(workspace.path(), &["checkout", "main"]);

    // Deliberately do NOT set BREHON_PROJECT_ROOT so the path-equality
    // check is skipped — we need the branch check to be the one that
    // catches this.
    let other_project = TempDir::new().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_PROJECT_ROOT",
        other_project.path().to_str().unwrap(),
    )]);

    let err = ensure_checkpoint_cwd_is_isolated(workspace.path()).unwrap_err();
    assert!(
        err.contains("default branch") || err.contains("on 'main'"),
        "expected default-branch rejection, got: {err}"
    );
}

#[test]
fn checkpoint_cwd_guard_accepts_isolated_worker_worktree() {
    // Positive control: cwd is not the project root and HEAD is on a
    // worker branch. Guard must allow it.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = TempDir::new().unwrap();
    init_git_workspace(workspace.path()); // leaves HEAD on worker/test
    let other_project = TempDir::new().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_PROJECT_ROOT",
        other_project.path().to_str().unwrap(),
    )]);

    ensure_checkpoint_cwd_is_isolated(workspace.path())
        .expect("guard should accept isolated worker worktree on a worker branch");
}

#[test]
fn current_workspace_root_trims_workspace_root_value() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = TempDir::new().unwrap();
    let wrapped = format!("  {}  ", workspace.path().display());
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_WORKSPACE_ROOT", &wrapped)]);

    assert_eq!(current_workspace_root().as_deref(), Ok(workspace.path()));
}

fn write_review_metadata_with_commits(
    brehon_root: &Path,
    task_id: &str,
    status: &str,
    commit: &str,
    commits: &[&str],
) {
    write_review_metadata_with_commits_and_empty_flag(
        brehon_root,
        task_id,
        status,
        commit,
        commits,
        false,
    );
}

fn write_review_metadata_with_commits_and_empty_flag(
    brehon_root: &Path,
    task_id: &str,
    status: &str,
    commit: &str,
    commits: &[&str],
    resolved_empty_commit_set: bool,
) {
    let reviews_dir = brehon_root.join("runtime").join("reviews").join(task_id);
    let round_dir = reviews_dir.join("round-1");
    std::fs::create_dir_all(&round_dir).unwrap();
    let state = serde_json::json!({
        "task_id": task_id,
        "status": status,
        "current_round": 1,
        "current_review_id": "REV-test",
        "max_rounds": 3,
        "panel_mode": "full_council",
        "panel": ["reviewer-1"],
        "submissions_received": ["reviewer-1"],
        "created_at": "2026-04-05T00:00:00Z",
        "updated_at": "2026-04-05T00:00:00Z"
    });
    std::fs::write(
        reviews_dir.join("state.json"),
        serde_json::to_string_pretty(&state).unwrap(),
    )
    .unwrap();
    let request = serde_json::json!({
        "task_id": task_id,
        "review_id": "REV-test",
        "requested_by": "supervisor-1",
        "requested_at": "2026-04-05T00:00:00Z",
        "title": format!("Task {task_id}"),
        "description": "test request",
        "commit": commit,
        "commits": commits,
        "resolved_empty_commit_set": resolved_empty_commit_set,
        "context": ""
    });
    std::fs::write(
        round_dir.join("request.json"),
        serde_json::to_string_pretty(&request).unwrap(),
    )
    .unwrap();
}

fn write_review_metadata(brehon_root: &Path, task_id: &str, status: &str, commit: &str) {
    write_review_metadata_with_commits(brehon_root, task_id, status, commit, &[]);
}

#[derive(Clone, Copy, Debug)]
enum IntegrationSequenceScenario {
    Clean,
    Conflict,
}

#[derive(Clone, Copy, Debug)]
enum IntegrationSequenceOperation {
    Integrate,
    AbortIntegration,
    ForceIntegrate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntegrationSequencePhase {
    Null,
    CherryPicking,
    Aborted,
    Complete,
    Resolved,
}

struct IntegrationSequenceFixture {
    _workspace: TempDir,
    _env: ScopedEnv,
    tool: TaskActionsTool,
    brehon_root: PathBuf,
    task_id: String,
}

impl IntegrationSequenceFixture {
    async fn for_scenario(scenario: IntegrationSequenceScenario) -> Self {
        let workspace = make_test_root();
        let brehon_root = workspace.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();

        init_git_workspace(workspace.path());
        run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
        if matches!(scenario, IntegrationSequenceScenario::Conflict) {
            std::fs::create_dir_all(workspace.path().join("src")).unwrap();
            std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
            run_git(workspace.path(), &["add", "src/conflict.txt"]);
            run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
        }
        run_git(workspace.path(), &["checkout", "main"]);

        run_git(
            workspace.path(),
            &["checkout", "-b", "worker/task-property-sequence"],
        );
        match scenario {
            IntegrationSequenceScenario::Clean => {
                std::fs::write(workspace.path().join("feature.txt"), "clean integrate\n").unwrap();
                run_git(workspace.path(), &["add", "feature.txt"]);
                run_git(workspace.path(), &["commit", "-m", "clean feature"]);
            }
            IntegrationSequenceScenario::Conflict => {
                std::fs::create_dir_all(workspace.path().join("src")).unwrap();
                std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n")
                    .unwrap();
                run_git(workspace.path(), &["add", "src/conflict.txt"]);
                run_git(workspace.path(), &["commit", "-m", "conflicting feature"]);
            }
        }
        let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
        run_git(workspace.path(), &["checkout", "main"]);

        let env = ScopedEnv::set_with_defaults(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
            ("BREHON_AGENT_ROLE", "supervisor"),
            ("BREHON_AGENT_NAME", "sup-1"),
            ("BREHON_SUPERVISOR_NAME", "sup-1"),
        ]);
        let tool = TaskActionsTool::new();

        let epic_json =
            create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
        let epic_id = epic_json["task_id"].as_str().unwrap();
        let subtask_json =
            create_subtask_for_test(&tool, "Property Sequence Subtask", epic_id).await;
        let task_id = subtask_json["task_id"].as_str().unwrap().to_string();

        let mut task = read_test_task(&brehon_root, &task_id);
        task["status"] = "approved".into();
        task["integration_status"] = "pending".into();
        std::fs::write(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
        write_review_metadata(&brehon_root, &task_id, "approved", &reviewed_commit);

        Self {
            _workspace: workspace,
            _env: env,
            tool,
            brehon_root,
            task_id,
        }
    }

    async fn execute(&self, operation: IntegrationSequenceOperation) {
        let args = match operation {
            IntegrationSequenceOperation::Integrate => serde_json::json!({
                "action": "integrate",
                "id": self.task_id
            }),
            IntegrationSequenceOperation::AbortIntegration => serde_json::json!({
                "action": "abort-integration",
                "id": self.task_id,
                "reason": "property test sequence"
            }),
            IntegrationSequenceOperation::ForceIntegrate => serde_json::json!({
                "action": "integrate",
                "id": self.task_id,
                "force": true
            }),
        };
        let _ = self.tool.execute(args).await;
    }

    fn phase(&self) -> IntegrationSequencePhase {
        integration_sequence_phase(&read_test_task(&self.brehon_root, &self.task_id))
    }
}

fn integration_sequence_phase(task: &Value) -> IntegrationSequencePhase {
    match task
        .get("integration")
        .and_then(|value| value.get("phase"))
        .and_then(Value::as_str)
        .unwrap_or("null")
    {
        "null" => IntegrationSequencePhase::Null,
        "cherry_picking" => IntegrationSequencePhase::CherryPicking,
        "aborted" => IntegrationSequencePhase::Aborted,
        "complete" => IntegrationSequencePhase::Complete,
        "resolved" => IntegrationSequencePhase::Resolved,
        other => panic!("unexpected integration phase in property test: {other}"),
    }
}

fn expected_sequence_phase(
    scenario: IntegrationSequenceScenario,
    current: IntegrationSequencePhase,
    operation: IntegrationSequenceOperation,
) -> IntegrationSequencePhase {
    match scenario {
        IntegrationSequenceScenario::Clean => match (current, operation) {
            (IntegrationSequencePhase::Null, IntegrationSequenceOperation::AbortIntegration) => {
                IntegrationSequencePhase::Null
            }
            (
                IntegrationSequencePhase::Null,
                IntegrationSequenceOperation::Integrate
                | IntegrationSequenceOperation::ForceIntegrate,
            ) => IntegrationSequencePhase::Complete,
            (IntegrationSequencePhase::Complete, _) => IntegrationSequencePhase::Complete,
            _ => panic!(
                "unexpected clean-sequence state: current={current:?}, operation={operation:?}"
            ),
        },
        IntegrationSequenceScenario::Conflict => match (current, operation) {
            (IntegrationSequencePhase::Null, IntegrationSequenceOperation::AbortIntegration) => {
                IntegrationSequencePhase::Null
            }
            (
                IntegrationSequencePhase::Null,
                IntegrationSequenceOperation::Integrate
                | IntegrationSequenceOperation::ForceIntegrate,
            ) => IntegrationSequencePhase::CherryPicking,
            (
                IntegrationSequencePhase::CherryPicking,
                IntegrationSequenceOperation::Integrate
                | IntegrationSequenceOperation::ForceIntegrate,
            ) => IntegrationSequencePhase::CherryPicking,
            (
                IntegrationSequencePhase::CherryPicking,
                IntegrationSequenceOperation::AbortIntegration,
            ) => IntegrationSequencePhase::Aborted,
            (
                IntegrationSequencePhase::Aborted,
                IntegrationSequenceOperation::Integrate
                | IntegrationSequenceOperation::AbortIntegration,
            ) => IntegrationSequencePhase::Aborted,
            (
                IntegrationSequencePhase::Aborted,
                IntegrationSequenceOperation::ForceIntegrate,
            ) => IntegrationSequencePhase::CherryPicking,
            _ => panic!(
                "unexpected conflicting-sequence state: current={current:?}, operation={operation:?}"
            ),
        },
    }
}

fn integration_sequence_strategy() -> impl Strategy<
    Value = (
        IntegrationSequenceScenario,
        Vec<IntegrationSequenceOperation>,
    ),
> {
    (
        prop_oneof![
            Just(IntegrationSequenceScenario::Clean),
            Just(IntegrationSequenceScenario::Conflict),
        ],
        proptest::collection::vec(
            prop_oneof![
                Just(IntegrationSequenceOperation::Integrate),
                Just(IntegrationSequenceOperation::AbortIntegration),
                Just(IntegrationSequenceOperation::ForceIntegrate),
            ],
            1..8,
        ),
    )
}

#[test]
fn property_test_real_git_sequences_only_reach_valid_integration_phase_edges() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut runner = TestRunner::new(ProptestConfig {
        cases: 8,
        ..ProptestConfig::default()
    });
    runner
        .run(
            &integration_sequence_strategy(),
            |(scenario, operations)| -> Result<(), TestCaseError> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let fixture = IntegrationSequenceFixture::for_scenario(scenario).await;
                    let mut expected_phase = IntegrationSequencePhase::Null;

                    for operation in operations {
                        let before = fixture.phase();
                        prop_assert_eq!(before, expected_phase, "scenario={:?}", scenario);

                        fixture.execute(operation).await;

                        let after = fixture.phase();
                        let next_expected =
                            expected_sequence_phase(scenario, expected_phase, operation);
                        prop_assert_eq!(after, next_expected, "scenario={:?}", scenario);
                        expected_phase = next_expected;
                    }

                    Ok(())
                })
            },
        )
        .unwrap();
}

async fn create_epic_for_test(
    tool: &TaskActionsTool,
    title: &str,
    integration_branch: Option<&str>,
) -> Value {
    ensure_test_project_repo();
    let mut args = serde_json::json!({
        "action": "create",
        "task_type": "epic",
        "title": title,
        "description": format!("{title} coordinates related implementation work."),
        "acceptance_criteria": [
            format!("{title} has a complete implementation plan"),
            format!("{title} can track subtask completion")
        ],
        "plan_steps": [
            "Create the epic".to_string(),
            "Track and validate subtask progress".to_string()
        ]
    });
    if let Some(branch) = integration_branch {
        args["integration_branch"] = serde_json::json!(branch);
    }
    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    serde_json::from_str(&extract_text(&result)).unwrap()
}

async fn create_plain_epic_for_test(tool: &TaskActionsTool, title: &str) -> Value {
    let result = tool
            .execute(serde_json::json!({
                "action": "create",
                "task_type": "epic",
                "title": title,
                "description": format!("{title} is an audit-only coordination epic with no code changes."),
                "acceptance_criteria": [
                    format!("{title} captures the audit scope"),
                    format!("{title} tracks close-only follow-up tasks")
                ],
                "plan_steps": [
                    "Review the current state",
                    "Track non-code follow-up"
                ]
            }))
            .await
            .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    serde_json::from_str(&extract_text(&result)).unwrap()
}

async fn create_initiative_for_test(tool: &TaskActionsTool, title: &str) -> Value {
    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "initiative",
            "title": title,
            "description": format!("{title} coordinates multiple phase epics."),
            "acceptance_criteria": [
                format!("{title} has phase-level acceptance criteria"),
                format!("{title} can track child epic completion")
            ],
            "plan_steps": [
                "Create phase epics",
                "Drive each phase to close"
            ],
            "implementation_notes": "Use one epic per phase."
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    serde_json::from_str(&extract_text(&result)).unwrap()
}

async fn create_subtask_for_test(tool: &TaskActionsTool, title: &str, parent_id: &str) -> Value {
    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": title,
            "parent_id": parent_id,
            "description": format!("{title} implements one concrete outcome."),
            "acceptance_criteria": [format!("{title} completes its assigned outcome")],
            "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
            "test_requirements": ["cargo test -p brehon-mcp"],
            "plan_steps": ["Inspect current state", "Implement the change", "Verify the result"]
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    serde_json::from_str(&extract_text(&result)).unwrap()
}

async fn create_direct_to_main_subtask_for_test(
    tool: &TaskActionsTool,
    title: &str,
    parent_id: &str,
) -> Value {
    let result = tool
            .execute(serde_json::json!({
                "action": "create",
                "title": title,
                "parent_id": parent_id,
                "direct_to_main": true,
                "description": format!("{title} implements one concrete outcome directly to the default branch."),
                "acceptance_criteria": [format!("{title} completes its assigned outcome")],
                "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
                "test_requirements": ["cargo test -p brehon-mcp"],
                "plan_steps": ["Inspect current state", "Implement the change", "Verify the result"]
            }))
            .await
            .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    serde_json::from_str(&extract_text(&result)).unwrap()
}

async fn create_standalone_task_for_test(tool: &TaskActionsTool, title: &str) -> Value {
    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": title,
            "description": format!("{title} performs one concrete implementation task."),
            "acceptance_criteria": [format!("{title} is complete")],
            "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
            "test_requirements": ["cargo test -p brehon-mcp"],
            "plan_steps": ["Read the target area", "Implement the task", "Run verification"]
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    serde_json::from_str(&extract_text(&result)).unwrap()
}

#[tokio::test]
async fn test_create_and_list() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let args = serde_json::json!({
        "action": "create",
        "title": "Test task",
        "description": "Implement structured task creation for merge-mode work.",
        "acceptance_criteria": [
            "Task creation accepts structured planning fields",
            "The rendered description preserves those sections"
        ],
        "file_hints": [
            "crates/brehon-mcp/src/tools/task_actions.rs",
            "crates/brehon-mcp/src/tools/agent.rs"
        ],
        "test_requirements": [
            "cargo test -p brehon-mcp test_create_and_list"
        ],
        "plan_steps": [
            "Parse structured task fields",
            "Render them into the task description"
        ],
        "priority": "high"
    });
    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());
    if let ContentBlock::Text { text } = &result.content[0] {
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["status"], "pending");
        assert!(v["task_id"].as_str().unwrap().starts_with("T-"));
        assert_eq!(v["completion_mode"], "merge");
        assert_eq!(v["acceptance_criteria"].as_array().unwrap().len(), 2);
        assert!(v["description"]
            .as_str()
            .unwrap_or("")
            .contains("Acceptance Criteria:"));
    }

    // List should show the task
    let list_result = tool
        .execute(serde_json::json!({ "action": "list" }))
        .await
        .unwrap();
    if let ContentBlock::Text { text } = &list_result.content[0] {
        let v: Value = serde_json::from_str(text).unwrap();
        assert!(v["count"].as_u64().unwrap() >= 1);
    }
}

#[tokio::test]
async fn test_create_initiative_and_phase_epic() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "Continuity Plan").await;
    let initiative_id = initiative["task_id"].as_str().unwrap();
    let initiative_branch = initiative["integration_branch"].as_str().unwrap();
    let initiative_worktree = initiative["integration_worktree"].as_str().unwrap();
    assert!(initiative_branch.starts_with("initiative/"));
    assert!(initiative_worktree.contains("worktrees/initiative"));
    assert!(Path::new(initiative_worktree).exists());

    let epic_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "parent_id": initiative_id,
            "title": "Phase 1",
            "description": "Phase 1 delivers the first coherent implementation slice.",
            "acceptance_criteria": ["Phase 1 closes with all worker tasks complete"],
            "plan_steps": ["Create tasks", "Run review", "Close phase"]
        }))
        .await
        .unwrap();
    assert!(
        epic_result.is_error.is_none(),
        "{}",
        extract_text(&epic_result)
    );
    let epic: Value = serde_json::from_str(&extract_text(&epic_result)).unwrap();
    assert_eq!(epic["task_type"], "epic");
    assert_eq!(epic["parent_id"], initiative_id);
    assert!(epic["integration_branch"]
        .as_str()
        .unwrap()
        .starts_with("epic/"));
    assert!(epic["integration_worktree"]
        .as_str()
        .unwrap()
        .contains("worktrees/epic"));

    let children = tool
        .execute(serde_json::json!({
            "action": "children",
            "id": initiative_id
        }))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&children)).unwrap();
    assert_eq!(payload["parent_type"], "initiative");
    assert_eq!(payload["child_type"], "epics");
    assert_eq!(payload["total"], 1);
}

#[tokio::test]
async fn test_create_container_worktree_honors_external_worktree_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let external = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_WORKTREE_ROOT", external.path().to_str().unwrap()),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "External Worktree Plan").await;
    let worktree = Path::new(initiative["integration_worktree"].as_str().unwrap());

    assert!(worktree.starts_with(external.path()));
    assert!(!worktree.starts_with(&brehon_root));
    assert!(worktree.exists());
}

#[tokio::test]
async fn test_ensure_final_hardening_backfills_and_is_idempotent() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "Continuity Plan").await;
    let initiative_id = initiative["task_id"].as_str().unwrap();
    let mut phase_epic_ids = Vec::new();
    for title in ["Phase 1", "Phase 2"] {
        let result = tool
            .execute(serde_json::json!({
                "action": "create",
                "task_type": "epic",
                "parent_id": initiative_id,
                "title": title,
                "description": format!("{title} delivers one implementation slice."),
                "acceptance_criteria": [format!("{title} closes cleanly")],
                "plan_steps": ["Create tasks", "Review work", "Close phase"]
            }))
            .await
            .unwrap();
        assert!(result.is_error.is_none(), "{}", extract_text(&result));
        let epic: Value = serde_json::from_str(&extract_text(&result)).unwrap();
        phase_epic_ids.push(epic["task_id"].as_str().unwrap().to_string());
    }

    let first = tool
        .execute(serde_json::json!({
            "action": "ensure_final_hardening",
            "id": initiative_id,
            "source_file": "plan.md",
            "role": "supervisor"
        }))
        .await
        .unwrap();
    assert!(first.is_error.is_none(), "{}", extract_text(&first));
    let first_json: Value = serde_json::from_str(&extract_text(&first)).unwrap();
    assert_eq!(first_json["epic"]["created"], true);
    assert_eq!(first_json["seed_tasks"].as_array().unwrap().len(), 3);
    assert!(first_json["seed_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["created"] == true));

    let hardening_epic_id = first_json["epic"]["task_id"].as_str().unwrap();
    let hardening_epic = read_test_task(&brehon_root, hardening_epic_id);
    assert_eq!(
        hardening_epic["title"],
        "Final Hardening and Cross-Epic Cleanup"
    );
    assert_eq!(
        hardening_epic["plan_import"]["kind"],
        "final_hardening_epic"
    );
    for phase_epic_id in &phase_epic_ids {
        assert!(hardening_epic["dependencies"]
            .as_array()
            .unwrap()
            .contains(&Value::String(phase_epic_id.clone())));
        assert!(hardening_epic["blocked_by"]
            .as_array()
            .unwrap()
            .contains(&Value::String(phase_epic_id.clone())));
    }

    let mut seed_ids = first_json["seed_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["task_id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    seed_ids.sort();
    let first_seed = read_test_task(
        &brehon_root,
        first_json["seed_tasks"][0]["task_id"].as_str().unwrap(),
    );
    assert_eq!(first_seed["completion_mode"], "close");
    assert_eq!(
        first_seed["execution_policy"]["work_class"],
        "final_hardening"
    );
    assert_eq!(
        first_seed["execution_policy"]["preferred_lane"],
        "codex-hardening"
    );
    assert_eq!(
        first_seed["execution_policy"]["preferred_agent_type"],
        "codex"
    );
    assert_eq!(first_seed["execution_policy"]["preferred_model"], "gpt-5.5");
    assert_eq!(
        first_seed["execution_policy"]["preferred_reasoning_effort"],
        "xhigh"
    );
    assert_eq!(first_seed["execution_policy"]["strict"], true);

    let second_seed = read_test_task(
        &brehon_root,
        first_json["seed_tasks"][1]["task_id"].as_str().unwrap(),
    );
    assert!(second_seed["dependencies"]
        .as_array()
        .unwrap()
        .contains(&first_json["seed_tasks"][0]["task_id"]));

    let second = tool
        .execute(serde_json::json!({
            "action": "ensure_final_hardening",
            "id": initiative_id,
            "source_file": "plan.md",
            "role": "supervisor"
        }))
        .await
        .unwrap();
    assert!(second.is_error.is_none(), "{}", extract_text(&second));
    let second_json: Value = serde_json::from_str(&extract_text(&second)).unwrap();
    assert_eq!(second_json["epic"]["created"], false);
    assert!(second_json["seed_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["created"] == false));
    let mut second_seed_ids = second_json["seed_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["task_id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    second_seed_ids.sort();
    assert_eq!(second_seed_ids, seed_ids);
}

#[tokio::test]
async fn test_reject_phase_epic_direct_to_main_under_initiative() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "Continuity Plan").await;
    let initiative_id = initiative["task_id"].as_str().unwrap();

    let epic_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "parent_id": initiative_id,
            "direct_to_main": true,
            "title": "Phase 1",
            "description": "Phase 1 delivers the first coherent implementation slice.",
            "acceptance_criteria": ["Phase 1 closes with all worker tasks complete"],
            "plan_steps": ["Create tasks", "Run review", "Close phase"]
        }))
        .await
        .unwrap();
    assert_eq!(epic_result.is_error, Some(true));
    assert!(
        extract_text(&epic_result)
            .contains("Epics under an initiative cannot use direct_to_main=true"),
        "{}",
        extract_text(&epic_result)
    );
}

#[tokio::test]
async fn test_reject_task_directly_under_initiative() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(root.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", root.path().to_str().unwrap()),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "Continuity Plan").await;
    let initiative_id = initiative["task_id"].as_str().unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "parent_id": initiative_id,
            "title": "Illegal direct task",
            "description": "This should be rejected.",
            "acceptance_criteria": ["not used"],
            "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
            "test_requirements": ["cargo test -p brehon-mcp"],
            "plan_steps": ["Reject it"]
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_some());
    assert!(extract_text(&result).contains("cannot contain child type=task"));
}

#[tokio::test]
async fn test_ready_excludes_tasks_under_closed_initiative() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "I-1", "closed", "initiative");
    write_test_task(root.path(), "E-1", "pending", "epic");
    write_test_task(root.path(), "T-1", "pending", "task");

    let mut epic = read_test_task(root.path(), "E-1");
    epic["parent_id"] = Value::String("I-1".to_string());
    std::fs::write(
        root.path().join("runtime").join("tasks").join("E-1.json"),
        serde_json::to_string_pretty(&epic).unwrap(),
    )
    .unwrap();

    let mut task = read_test_task(root.path(), "T-1");
    task["parent_id"] = Value::String("E-1".to_string());
    task["assignee"] = Value::Null;
    std::fs::write(
        root.path().join("runtime").join("tasks").join("T-1.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 0);
}

#[tokio::test]
async fn test_ready_excludes_control_plane_tasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-worker", "pending", "task");
    let mut task = read_test_task(root.path(), "T-worker");
    task["assignee"] = Value::Null;
    task["file_hints"] = serde_json::json!([".brehon/runtime/reviews/T-1/state.json"]);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-worker.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 0);
}

#[tokio::test]
async fn test_ready_rejects_invalid_project_routing_config() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = make_test_root();
    let brehon_root = project.path().join(".brehon");
    let config_dir = project.path().join("xdg");
    std::fs::create_dir_all(&brehon_root).unwrap();
    std::fs::write(
        brehon_root.join("config.yaml"),
        r#"
routing:
  default_worker_lane: missing-worker
"#,
    )
    .unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("XDG_CONFIG_HOME", config_dir.to_str().unwrap()),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(&brehon_root, "T-ready", "pending", "task");

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();

    assert_eq!(ready.is_error, Some(true));
    let text = extract_text(&ready);
    assert!(
        text.contains("project config is invalid"),
        "expected invalid config error, got: {text}"
    );
    assert!(
        text.contains("routing.default_worker_lane"),
        "expected routing lane detail, got: {text}"
    );
}

#[tokio::test]
async fn test_conflicts_lists_supervisor_owned_integration_conflicts() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-conflict", "changes_requested", "task");
    write_test_task(root.path(), "T-normal", "changes_requested", "task");

    let mut conflict = read_test_task(root.path(), "T-conflict");
    conflict["integration_conflict"] = serde_json::json!({
        "owner": "supervisor",
        "source": "approved_integration",
        "merge_target": "epic/test",
        "reviewed_commit": "deadbeef",
        "conflicting_files": ["Cargo.toml", "Cargo.lock"]
    });
    conflict["updated_at"] = Value::String("2026-04-10T01:11:54Z".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-conflict.json"),
        serde_json::to_string_pretty(&conflict).unwrap(),
    )
    .unwrap();

    let conflicts = tool
        .execute(serde_json::json!({"action": "conflicts"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&conflicts)).unwrap();
    assert_eq!(payload["count"], 1);
    assert_eq!(payload["tasks"][0]["task_id"], "T-conflict");
    assert_eq!(
        payload["tasks"][0]["integration_conflict"]["owner"],
        "supervisor"
    );
}

#[tokio::test]
async fn test_ready_surfaces_supervisor_owned_integration_conflicts_separately() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-pending", "pending", "task");
    write_test_task(root.path(), "T-conflict", "changes_requested", "task");

    let mut pending = read_test_task(root.path(), "T-pending");
    pending["assignee"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-pending.json"),
        serde_json::to_string_pretty(&pending).unwrap(),
    )
    .unwrap();

    let mut conflict = read_test_task(root.path(), "T-conflict");
    conflict["assignee"] = Value::Null;
    conflict["integration_conflict"] = serde_json::json!({
        "owner": "supervisor",
        "source": "approved_integration",
        "merge_target": "epic/test",
        "reviewed_commit": "deadbeef",
        "conflicting_files": ["Cargo.toml", "Cargo.lock"]
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-conflict.json"),
        serde_json::to_string_pretty(&conflict).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-pending");
    assert_eq!(payload["changes_requested_count"], 0, "{payload}");
    assert_eq!(payload["integration_conflict_count"], 1, "{payload}");
    assert_eq!(
        payload["integration_conflict_tasks"][0]["task_id"],
        "T-conflict"
    );
    assert!(payload["message"]
        .as_str()
        .unwrap_or_default()
        .contains("Resolve supervisor-owned integration conflicts"));
}

#[test]
fn default_conflict_owner_routes_review_preflight_to_worker() {
    // review_preflight conflicts are almost always worker-fixable via
    // local rebase. Marking them supervisor-owned floods the supervisor
    // queue with conflicts they cannot meaningfully act on. Ownership
    // for those defaults to the worker so the existing
    // assignee-preserving lifecycle keeps the worker actionable.
    assert_eq!(default_conflict_owner("review_preflight"), "worker");
}

#[test]
fn default_conflict_owner_routes_epic_worktree_conflicts_to_supervisor() {
    // approved_integration cherry-picks happen inside an
    // .brehon/worktrees/epic/* worktree only the supervisor can write to,
    // and worker_unmerged means the worker's own branch is in a state
    // Brehon can't auto-recover from. Both must escalate.
    assert_eq!(default_conflict_owner("approved_integration"), "supervisor");
    assert_eq!(default_conflict_owner("worker_unmerged"), "supervisor");
}

#[test]
fn default_conflict_owner_falls_back_to_supervisor_for_unknown_sources() {
    // Defense-in-depth: an unrecognised source could be a future
    // failure mode we haven't classified yet. Default to supervisor so
    // it gets visible attention rather than silently routing to the
    // worker who may not know how to act.
    assert_eq!(default_conflict_owner(""), "supervisor");
    assert_eq!(default_conflict_owner("some_future_source"), "supervisor");
}

#[tokio::test]
async fn test_apply_supervisor_integration_conflict_uses_source_aware_owner() {
    // Integration test for the writer end-to-end: feeding source =
    // review_preflight produces owner=worker on disk, while
    // approved_integration produces owner=supervisor.
    let mut preflight_task = serde_json::Map::new();
    preflight_task.insert("task_id".into(), Value::String("T-pf".into()));
    preflight_task.insert("assignee".into(), Value::String("worker-9".into()));
    apply_supervisor_integration_conflict(
        &mut preflight_task,
        "changes_requested",
        "epic/feature",
        "deadbeef",
        &["deadbeef".to_string()],
        &["src/x.txt".to_string()],
        "review_preflight",
        Some("worker-9"),
    );
    assert_eq!(
        preflight_task["integration_conflict"]["owner"], "worker",
        "review_preflight must default to worker ownership"
    );
    assert_eq!(preflight_task["assignee"], "worker-9");

    let mut approved_task = serde_json::Map::new();
    approved_task.insert("task_id".into(), Value::String("T-ai".into()));
    approved_task.insert("assignee".into(), Value::String("worker-9".into()));
    apply_supervisor_integration_conflict(
        &mut approved_task,
        "changes_requested",
        "epic/feature",
        "deadbeef",
        &["deadbeef".to_string()],
        &["src/x.txt".to_string()],
        "approved_integration",
        Some("worker-9"),
    );
    assert_eq!(
        approved_task["integration_conflict"]["owner"], "supervisor",
        "approved_integration must stay supervisor-owned"
    );
}

#[tokio::test]
async fn test_clear_supervisor_integration_conflict_restores_previous_worker() {
    // Regression test: integration-conflict apply parks the worker in
    // `integration_conflict.previous_worker`. Conflict clear must restore
    // `assignee`/`review_owner` so the worker's context can later be
    // recycled when the task reaches a terminal state.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

    write_test_task(root.path(), "T-conflict", "changes_requested", "task");

    let mut conflict = read_test_task(root.path(), "T-conflict");
    conflict["assignee"] = Value::Null;
    conflict["review_owner"] = Value::Null;
    conflict["integration_conflict"] = serde_json::json!({
        "owner": "supervisor",
        "source": "approved_integration",
        "merge_target": "epic/test",
        "reviewed_commit": "deadbeef",
        "conflicting_files": ["crates/brehon-tui/src/run/mod.rs"],
        "previous_worker": "glad-bee-22",
    });
    conflict["blockers"] = Value::String(format!(
        "{INTEGRATION_CONFLICT_BLOCKER_PREFIX}: resolve manually"
    ));
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-conflict.json"),
        serde_json::to_string_pretty(&conflict).unwrap(),
    )
    .unwrap();

    clear_task_supervisor_integration_conflict("T-conflict")
        .await
        .expect("clear should succeed");

    let after = read_test_task(root.path(), "T-conflict");
    assert!(
        after.get("integration_conflict").is_none(),
        "integration_conflict blob should be removed"
    );
    assert_eq!(
        after["assignee"], "glad-bee-22",
        "assignee should be restored from previous_worker"
    );
    assert_eq!(
        after["review_owner"], "glad-bee-22",
        "review_owner should be restored from previous_worker"
    );
    assert!(
        after.get("blockers").is_none(),
        "supervisor-conflict blockers should be cleared"
    );
}

#[tokio::test]
async fn test_clear_supervisor_integration_conflict_preserves_existing_assignee() {
    // If the supervisor reassigned the task during conflict resolution,
    // clear must not overwrite the new assignee with the stale previous_worker.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

    write_test_task(root.path(), "T-conflict", "changes_requested", "task");

    let mut conflict = read_test_task(root.path(), "T-conflict");
    conflict["assignee"] = Value::String("new-owner-99".to_string());
    conflict["review_owner"] = Value::String("new-owner-99".to_string());
    conflict["integration_conflict"] = serde_json::json!({
        "owner": "supervisor",
        "source": "approved_integration",
        "merge_target": "epic/test",
        "reviewed_commit": "deadbeef",
        "conflicting_files": [],
        "previous_worker": "glad-bee-22",
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-conflict.json"),
        serde_json::to_string_pretty(&conflict).unwrap(),
    )
    .unwrap();

    clear_task_supervisor_integration_conflict("T-conflict")
        .await
        .expect("clear should succeed");

    let after = read_test_task(root.path(), "T-conflict");
    assert_eq!(
        after["assignee"], "new-owner-99",
        "existing assignee must not be clobbered by previous_worker"
    );
    assert_eq!(
        after["review_owner"], "new-owner-99",
        "existing review_owner must not be clobbered"
    );
}

#[tokio::test]
async fn test_supervisor_can_recover_blocked_integration_conflict_to_review_ready() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-blocked-conflict", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-blocked-conflict");
    task["assignee"] = Value::Null;
    task["review_owner"] = Value::Null;
    task["activity"] = Value::String("integration_conflict".to_string());
    task["blockers"] = Value::String(format!(
        "{INTEGRATION_CONFLICT_BLOCKER_PREFIX} for reviewed commit deadbeef against 'epic/test'."
    ));
    task["integration_conflict"] = serde_json::json!({
        "owner": "supervisor",
        "source": "review_preflight",
        "merge_target": "epic/test",
        "reviewed_commit": "deadbeef",
        "previous_worker": "worker-1",
        "conflicting_files": ["src/lib.rs"]
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-blocked-conflict.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-blocked-conflict",
            "status": "review_ready"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let after = read_test_task(root.path(), "T-blocked-conflict");
    assert_eq!(after["status"], "review_ready");
    assert!(
        after.get("integration_conflict").is_none(),
        "stale integration conflict marker should be cleared"
    );
    assert!(
        after.get("blockers").is_none(),
        "stale integration blocker text should be cleared"
    );
    assert_eq!(after["assignee"], "worker-1");
    assert_eq!(after["review_owner"], "worker-1");
}

#[tokio::test]
async fn test_blocked_without_integration_conflict_cannot_jump_to_review_ready() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-manual-blocked", "blocked", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-manual-blocked",
            "status": "review_ready"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("Invalid status transition"),
        "{}",
        extract_text(&result)
    );
    let after = read_test_task(root.path(), "T-manual-blocked");
    assert_eq!(after["status"], "blocked");
}

#[tokio::test]
async fn test_supervisor_can_recover_blocked_worker_handoff_to_review_ready() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-blocked-handoff", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-blocked-handoff");
    task["latest_commit"] = Value::String("deadbeef".to_string());
    task["percent"] = Value::Number(serde_json::Number::from(95_u64));
    task["blockers"] = Value::String(
        "Checkpoint succeeded, but task action=complete could not move it to review_ready: \
         Invalid status transition: 'blocked' → 'in_progress'. Valid transitions from 'blocked': pending."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-blocked-handoff.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-blocked-handoff",
            "status": "review_ready"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let after = read_test_task(root.path(), "T-blocked-handoff");
    assert_eq!(after["status"], "review_ready");
    assert_eq!(after["percent"], 100);
    assert_eq!(after["activity"], "awaiting_review");
    assert!(after.get("blockers").is_none());
    assert_eq!(after["assignee"], Value::Null);
    assert_eq!(after["review_owner"], Value::Null);
}

#[tokio::test]
async fn test_supervisor_cannot_recover_blocked_worker_handoff_without_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-blocked-no-commit", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-blocked-no-commit");
    task["blockers"] = Value::String(
        "Checkpoint succeeded, but task action=complete could not move it to review_ready: \
         Invalid status transition: 'blocked' → 'in_progress'. Valid transitions from 'blocked': pending."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-blocked-no-commit.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-blocked-no-commit",
            "status": "review_ready"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("latest_commit is empty"),
        "{}",
        extract_text(&result)
    );
    let after = read_test_task(root.path(), "T-blocked-no-commit");
    assert_eq!(after["status"], "blocked");
}

#[tokio::test]
async fn test_recover_handoff_action_repairs_blocked_worker_handoff() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-recover-action", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-recover-action");
    task["latest_commit"] = Value::String("feedface".to_string());
    task["percent"] = Value::Number(serde_json::Number::from(90_u64));
    task["blockers"] = Value::String(
        "Checkpoint succeeded, but task action=complete could not move it to review_ready: \
         Invalid status transition: 'blocked' -> 'in_progress'."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-recover-action.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-recover-action"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["action"], "recover_handoff");
    assert_eq!(payload["from_status"], "blocked");
    assert_eq!(payload["to_status"], "review_ready");
    assert_eq!(payload["latest_commit"], "feedface");
    assert_eq!(payload["next_action"]["kind"], "request_review");

    let after = read_test_task(root.path(), "T-recover-action");
    assert_eq!(after["status"], "review_ready");
    assert_eq!(after["percent"], 100);
    assert_eq!(after["activity"], "awaiting_review");
    assert!(after.get("blockers").is_none());
    assert_eq!(after["assignee"], Value::Null);
    assert_eq!(after["review_owner"], Value::Null);
}

#[tokio::test]
async fn test_recover_handoff_action_repairs_legacy_completed_worker_handoff() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-completed-action", "completed", "task");
    let mut task = read_test_task(root.path(), "T-completed-action");
    task["latest_commit"] = Value::String("feedface".to_string());
    task["percent"] = Value::Number(serde_json::Number::from(100_u64));
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-completed-action.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-completed-action"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["from_status"], "completed");
    assert_eq!(payload["to_status"], "review_ready");
    assert_eq!(payload["next_action"]["kind"], "request_review");

    let after = read_test_task(root.path(), "T-completed-action");
    assert_eq!(after["status"], "review_ready");
    assert_eq!(after["latest_commit"], "feedface");
    assert_eq!(after["assignee"], Value::Null);
    assert_eq!(after["review_owner"], Value::Null);
}

#[tokio::test]
async fn test_repair_frontier_repairs_all_safe_blocked_handoffs() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    for task_id in ["T-repair-a", "T-repair-b"] {
        write_test_task(root.path(), task_id, "blocked", "task");
        let mut task = read_test_task(root.path(), task_id);
        task["latest_commit"] = Value::String(format!("{task_id}-commit"));
        task["blockers"] = Value::String(
            "State deadlock: checkpoint created during pending state, need reassignment to complete"
                .to_string(),
        );
        std::fs::write(
            root.path()
                .join("runtime")
                .join("tasks")
                .join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
    }

    let result = tool
        .execute(serde_json::json!({ "action": "repair_frontier" }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["action"], "repair_frontier");
    assert_eq!(payload["repaired_count"], 2, "{payload}");
    assert_eq!(payload["skipped_count"], 0, "{payload}");
    assert_eq!(payload["next_action"]["args"]["action"], "ready");
    assert_eq!(
        read_test_task(root.path(), "T-repair-a")["status"],
        "review_ready"
    );
    assert_eq!(
        read_test_task(root.path(), "T-repair-b")["status"],
        "review_ready"
    );
}

#[tokio::test]
async fn test_recover_handoff_returns_structured_error_without_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-no-commit-action", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-no-commit-action");
    task["blockers"] = Value::String(
        "State deadlock: checkpoint created during pending state, need reassignment to complete"
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-no-commit-action.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-no-commit-action"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["error_code"], "handoff_missing_latest_commit");
    assert_eq!(payload["retryable"], false);
    assert_eq!(payload["current_state"]["status"], "blocked");
    assert!(payload["current_state"]["latest_commit"].is_null());
    assert_eq!(payload["next_action"]["args"]["action"], "ready");
    assert!(!payload["allowed_next_actions"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        read_test_task(root.path(), "T-no-commit-action")["status"],
        "blocked"
    );
}

#[tokio::test]
async fn test_recover_handoff_rejects_non_blocked_task_with_structured_error() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-wrong-status", "pending", "task");
    let mut task = read_test_task(root.path(), "T-wrong-status");
    task["latest_commit"] = Value::String("abc123".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-wrong-status.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-wrong-status"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["error_code"], "handoff_wrong_status");
    assert_eq!(payload["current_state"]["status"], "pending");
    assert_eq!(payload["next_action"]["args"]["action"], "ready");
}

#[tokio::test]
async fn test_repair_frontier_requires_supervisor_with_structured_error() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();

    let result = tool
        .execute(serde_json::json!({ "action": "repair_frontier" }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["error_code"], "supervisor_required");
    assert_eq!(payload["retryable"], false);
    assert_eq!(payload["current_state"]["caller_role"], "worker");
    assert_eq!(payload["next_action"]["args"]["action"], "ready");
}

#[tokio::test]
async fn test_recover_handoff_reports_structured_errors_for_unsafe_states() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();

    {
        let _env = ScopedEnv::set_with_defaults(&[
            ("BREHON_ROOT", root.path().to_str().unwrap()),
            ("BREHON_AGENT_ROLE", "worker"),
        ]);
        let tool = TaskActionsTool::new();
        let result = tool
            .execute(serde_json::json!({
                "action": "recover_handoff",
                "id": "T-any"
            }))
            .await
            .unwrap();
        let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(payload["error_code"], "supervisor_required");
        assert_eq!(payload["next_action"]["args"]["action"], "ready");
    }

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    let assert_error_code = |result: ToolResult, expected: &str| -> Value {
        assert_eq!(result.is_error, Some(true), "{}", extract_text(&result));
        let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(payload["error_code"], expected, "{payload}");
        assert_eq!(payload["next_action"]["args"]["action"], "ready");
        payload
    };

    let missing_id = tool
        .execute(serde_json::json!({ "action": "recover_handoff" }))
        .await
        .unwrap();
    assert_error_code(missing_id, "missing_task_id");

    let missing_task = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-missing"
        }))
        .await
        .unwrap();
    let payload = assert_error_code(missing_task, "task_not_found");
    assert_eq!(payload["current_state"]["exists"], false);

    write_test_task(root.path(), "T-terminal-recover", "closed", "task");
    let terminal = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-terminal-recover"
        }))
        .await
        .unwrap();
    assert_error_code(terminal, "task_terminal");

    write_test_task(root.path(), "T-non-task-recover", "blocked", "epic");
    let mut non_task = read_test_task(root.path(), "T-non-task-recover");
    non_task["latest_commit"] = Value::String("abc123".to_string());
    non_task["blockers"] = Value::String(
        "State deadlock: checkpoint created during pending state, need reassignment to complete"
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-non-task-recover.json"),
        serde_json::to_string_pretty(&non_task).unwrap(),
    )
    .unwrap();
    let non_task_result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-non-task-recover"
        }))
        .await
        .unwrap();
    assert_error_code(non_task_result, "handoff_not_worker_task");

    write_test_task(root.path(), "T-not-recoverable", "blocked", "task");
    let mut not_recoverable = read_test_task(root.path(), "T-not-recoverable");
    not_recoverable["latest_commit"] = Value::String("def456".to_string());
    not_recoverable["blockers"] = Value::String("Waiting on external dependency".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-not-recoverable.json"),
        serde_json::to_string_pretty(&not_recoverable).unwrap(),
    )
    .unwrap();
    let not_recoverable_result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-not-recoverable"
        }))
        .await
        .unwrap();
    assert_error_code(not_recoverable_result, "handoff_not_recoverable");

    write_test_task(root.path(), "T-already-integrated", "blocked", "task");
    let mut already_integrated = read_test_task(root.path(), "T-already-integrated");
    already_integrated["latest_commit"] = Value::String("abc123".to_string());
    already_integrated["integration_status"] = Value::String("integrated".to_string());
    already_integrated["blockers"] = Value::String(
        "State deadlock: checkpoint created during pending state, need reassignment to complete"
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-already-integrated.json"),
        serde_json::to_string_pretty(&already_integrated).unwrap(),
    )
    .unwrap();
    let already_integrated_result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-already-integrated"
        }))
        .await
        .unwrap();
    assert_error_code(already_integrated_result, "handoff_already_integrated");

    write_test_task(root.path(), "T-rejected-review", "blocked", "task");
    let mut rejected_review = read_test_task(root.path(), "T-rejected-review");
    rejected_review["latest_commit"] = Value::String("def456".to_string());
    rejected_review["review_feedback"] = serde_json::json!({
        "outcome": "rejected",
        "review_id": "REV-wrong-commit"
    });
    rejected_review["blockers"] = Value::String(
        "State deadlock: checkpoint created during pending state, need reassignment to complete"
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-rejected-review.json"),
        serde_json::to_string_pretty(&rejected_review).unwrap(),
    )
    .unwrap();
    let rejected_review_result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-rejected-review"
        }))
        .await
        .unwrap();
    assert_error_code(rejected_review_result, "handoff_final_review_state");

    write_test_task(root.path(), "T-closed-parent", "closed", "epic");
    write_test_task(root.path(), "T-child-closed-parent", "blocked", "task");
    let mut child = read_test_task(root.path(), "T-child-closed-parent");
    child["parent_id"] = Value::String("T-closed-parent".to_string());
    child["latest_commit"] = Value::String("feedface".to_string());
    child["blockers"] = Value::String(
        "State deadlock: checkpoint created during pending state, need reassignment to complete"
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-child-closed-parent.json"),
        serde_json::to_string_pretty(&child).unwrap(),
    )
    .unwrap();
    let closed_parent_result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-child-closed-parent"
        }))
        .await
        .unwrap();
    assert_error_code(closed_parent_result, "handoff_closed_parent");

    write_test_task(root.path(), "T-control-plane-recover", "blocked", "task");
    let mut control_plane = read_test_task(root.path(), "T-control-plane-recover");
    control_plane["description"] = Value::String("Repair .brehon/runtime/tasks state".to_string());
    control_plane["latest_commit"] = Value::String("c0ffee".to_string());
    control_plane["blockers"] = Value::String(
        "State deadlock: checkpoint created during pending state, need reassignment to complete"
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-control-plane-recover.json"),
        serde_json::to_string_pretty(&control_plane).unwrap(),
    )
    .unwrap();
    let control_plane_result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-control-plane-recover"
        }))
        .await
        .unwrap();
    assert_error_code(control_plane_result, "handoff_control_plane_scope");
}

#[tokio::test]
async fn test_live_runtime_ready_and_repair_frontier_without_manual_task_json_edits() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let _supervisor_env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "supervisor-1"),
    ]);
    let tool = TaskActionsTool::new();
    let created = create_standalone_task_for_test(&tool, "Live blocked handoff repair").await;
    let task_id = created["task_id"].as_str().unwrap().to_string();

    let blocked = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": task_id,
            "status": "blocked",
            "blockers": "State deadlock: checkpoint created during pending state, need reassignment to complete",
            "role": "supervisor"
        }))
        .await
        .unwrap();
    assert!(blocked.is_error.is_none(), "{}", extract_text(&blocked));
    let factory = crate::tools::factory::FactoryTool::new();
    let owned = factory
        .execute(serde_json::json!({
            "action": "set_ownership",
            "task_id": task_id,
            "worker": "worker-1"
        }))
        .await
        .unwrap();
    assert!(owned.is_error.is_none(), "{}", extract_text(&owned));
    drop(_supervisor_env);

    let _worker_env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "supervisor-1"),
    ]);
    std::fs::write(workspace.path().join("feature.txt"), "finished\n").unwrap();
    let checkpoint = tool
        .execute(serde_json::json!({
            "action": "checkpoint",
            "id": task_id,
            "message": "finished implementation",
            "role": "worker",
            "agent_name": "worker-1"
        }))
        .await
        .unwrap();
    assert!(
        checkpoint.is_error.is_none(),
        "{}",
        extract_text(&checkpoint)
    );
    drop(_worker_env);

    let _supervisor_env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "supervisor-1"),
    ]);
    let ready = tool
        .execute(serde_json::json!({ "action": "ready" }))
        .await
        .unwrap();
    assert!(ready.is_error.is_none(), "{}", extract_text(&ready));
    let ready_payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(
        ready_payload["recoverable_blocked_count"], 1,
        "{ready_payload}"
    );
    assert_eq!(
        ready_payload["next_action"]["args"]["action"],
        "repair_frontier"
    );

    let repaired = tool
        .execute(serde_json::json!({
            "action": "repair_frontier",
            "role": "supervisor"
        }))
        .await
        .unwrap();
    assert!(repaired.is_error.is_none(), "{}", extract_text(&repaired));
    let repaired_payload: Value = serde_json::from_str(&extract_text(&repaired)).unwrap();
    assert_eq!(repaired_payload["repaired_count"], 1, "{repaired_payload}");

    let after_ready = tool
        .execute(serde_json::json!({ "action": "ready" }))
        .await
        .unwrap();
    let after_payload: Value = serde_json::from_str(&extract_text(&after_ready)).unwrap();
    assert_eq!(
        after_payload["recoverable_blocked_count"], 0,
        "{after_payload}"
    );
    assert_eq!(after_payload["review_ready_count"], 1, "{after_payload}");
    assert_eq!(
        after_payload["review_ready_tasks"][0]["task_id"],
        Value::String(task_id)
    );
}

#[tokio::test]
async fn test_ready_reconciles_dependency_states_before_listing() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-blocker", "closed", "task");
    write_test_task(root.path(), "T-dependent", "blocked", "task");

    let mut dependent = read_test_task(root.path(), "T-dependent");
    dependent["dependencies"] = serde_json::json!(["T-blocker"]);
    dependent["blocked_by"] = serde_json::json!(["T-blocker"]);
    dependent["assignee"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-dependent.json"),
        serde_json::to_string_pretty(&dependent).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-dependent");

    let updated = read_test_task(root.path(), "T-dependent");
    assert_eq!(updated["status"], "pending");
    assert!(
        updated.get("blocked_by").is_none() || updated["blocked_by"] == serde_json::json!([]),
        "blocked_by should be empty after reconciliation: {updated:?}"
    );
}

#[tokio::test]
async fn test_ready_surfaces_review_ready_tasks_separately() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-pending", "pending", "task");
    write_test_task(root.path(), "T-review", "review_ready", "task");

    let mut pending = read_test_task(root.path(), "T-pending");
    pending["assignee"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-pending.json"),
        serde_json::to_string_pretty(&pending).unwrap(),
    )
    .unwrap();

    let mut review_ready = read_test_task(root.path(), "T-review");
    review_ready["assignee"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-review.json"),
        serde_json::to_string_pretty(&review_ready).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-pending");
    assert_eq!(payload["review_ready_count"], 1, "{payload}");
    assert_eq!(payload["review_ready_tasks"][0]["task_id"], "T-review");
    assert_eq!(payload["changes_requested_count"], 0, "{payload}");
}

#[tokio::test]
async fn test_ready_surfaces_unassigned_changes_requested_tasks_separately() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-pending", "pending", "task");
    write_test_task(root.path(), "T-revision", "changes_requested", "task");

    let mut pending = read_test_task(root.path(), "T-pending");
    pending["assignee"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-pending.json"),
        serde_json::to_string_pretty(&pending).unwrap(),
    )
    .unwrap();

    let mut revision = read_test_task(root.path(), "T-revision");
    revision["assignee"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-revision.json"),
        serde_json::to_string_pretty(&revision).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-pending");
    assert_eq!(payload["review_ready_count"], 0, "{payload}");
    assert_eq!(payload["approved_count"], 0, "{payload}");
    assert_eq!(payload["changes_requested_count"], 1, "{payload}");
    assert_eq!(
        payload["changes_requested_tasks"][0]["task_id"],
        "T-revision"
    );
}

#[tokio::test]
async fn test_ready_surfaces_duplicate_active_worker_assignments() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-review-a", "in_review", "task");
    write_test_task(root.path(), "T-review-b", "changes_requested", "task");

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();

    assert_eq!(
        payload["active_worker_assignment_conflict_count"], 1,
        "{payload}"
    );
    let conflict = &payload["active_worker_assignment_conflicts"][0];
    assert_eq!(conflict["worker"], "worker-1");
    assert_eq!(conflict["task_count"], 2);
    let task_ids = conflict["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|task| task["task_id"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert!(task_ids.contains("T-review-a"), "{conflict}");
    assert!(task_ids.contains("T-review-b"), "{conflict}");
    assert!(
        payload["message"]
            .as_str()
            .unwrap()
            .contains("worker assignment invariant conflict"),
        "{payload}"
    );
}

#[tokio::test]
async fn test_ready_surfaces_recoverable_blocked_worker_handoffs() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-handoff", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-handoff");
    task["latest_commit"] = Value::String("deadbeef".to_string());
    task["blockers"] = Value::String(
        "Checkpoint succeeded, but task action=complete could not move it to review_ready: \
         Invalid status transition: 'blocked' → 'in_progress'. Valid transitions from 'blocked': pending."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-handoff.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["recoverable_blocked_count"], 1, "{payload}");
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["task_id"],
        "T-handoff"
    );
    assert_eq!(payload["next_action"]["kind"], "repair_frontier");
    assert_eq!(payload["next_action"]["tool"], "task");
    assert!(
        payload["next_action"]["description"]
            .as_str()
            .unwrap()
            .contains("Apply deterministic safe repairs"),
        "{payload}"
    );
    assert_eq!(payload["next_action"]["args"]["action"], "repair_frontier");
    assert_eq!(payload["blocked_handoff_count"], 1, "{payload}");
    assert_eq!(payload["blocked_handoff_tasks"][0]["safe_repair"], true);
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["action"],
        "recover_handoff"
    );
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["id"],
        "T-handoff"
    );
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["liveness"]["state"],
        "missing_session"
    );
}

#[tokio::test]
async fn test_ready_does_not_mark_integrated_or_final_review_handoffs_safe() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    for (task_id, extra) in [
        (
            "T-integrated-handoff",
            serde_json::json!({ "integration_status": "integrated" }),
        ),
        (
            "T-rejected-handoff",
            serde_json::json!({ "review_feedback": { "outcome": "rejected" } }),
        ),
    ] {
        write_test_task(root.path(), task_id, "blocked", "task");
        let mut task = read_test_task(root.path(), task_id);
        task["latest_commit"] = Value::String(format!("{task_id}-commit"));
        task["blockers"] = Value::String(
            "State deadlock: checkpoint created during pending state, need reassignment to complete"
                .to_string(),
        );
        if let Some(map) = extra.as_object() {
            for (key, value) in map {
                task[key] = value.clone();
            }
        }
        std::fs::write(
            root.path()
                .join("runtime")
                .join("tasks")
                .join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
    }

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["blocked_handoff_count"], 2, "{payload}");
    assert_eq!(payload["recoverable_blocked_count"], 0, "{payload}");
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["safe_repair"],
        Value::Bool(false),
        "{payload}"
    );
    assert_eq!(
        payload["blocked_handoff_tasks"][1]["safe_repair"],
        Value::Bool(false),
        "{payload}"
    );
    let blockers = payload["blocked_handoff_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|task| task["repair_blocker"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        blockers.contains("integration_status=integrated"),
        "{payload}"
    );
    assert!(
        blockers.contains("review_feedback outcome=rejected"),
        "{payload}"
    );
}

#[tokio::test]
async fn test_ready_surfaces_legacy_completed_handoff_without_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-completed-no-commit", "completed", "task");
    let mut task = read_test_task(root.path(), "T-completed-no-commit");
    task["percent"] = Value::Number(serde_json::Number::from(100_u64));
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-completed-no-commit.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();

    assert_eq!(payload["count"], 0, "{payload}");
    assert_eq!(payload["blocked_handoff_count"], 1, "{payload}");
    assert_eq!(payload["recoverable_blocked_count"], 0, "{payload}");
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["task_id"],
        "T-completed-no-commit"
    );
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_blocker"],
        "latest_commit is missing"
    );
    assert_eq!(
        payload["next_action"]["kind"],
        "wait_for_worker_checkpoint_or_reassign"
    );
    assert!(
        payload["message"]
            .as_str()
            .unwrap()
            .contains("worker handoff task"),
        "{payload}"
    );
}

#[tokio::test]
async fn test_ready_surfaces_approved_merge_tasks_separately() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-approved", "approved", "task");

    let mut approved = read_test_task(root.path(), "T-approved");
    approved["completion_mode"] = Value::String("merge".to_string());
    approved["merge_target"] = Value::String("epic/test".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-approved.json"),
        serde_json::to_string_pretty(&approved).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 0, "{payload}");
    assert_eq!(payload["review_ready_count"], 0, "{payload}");
    assert_eq!(payload["changes_requested_count"], 0, "{payload}");
    assert_eq!(payload["approved_count"], 1, "{payload}");
    assert_eq!(payload["approved_tasks"][0]["task_id"], "T-approved");
}

#[tokio::test]
async fn test_ready_surfaces_open_followup_sources_separately() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-pending", "pending", "task");
    write_test_task(root.path(), "T-source", "integrated", "task");

    let mut pending = read_test_task(root.path(), "T-pending");
    pending["assignee"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-pending.json"),
        serde_json::to_string_pretty(&pending).unwrap(),
    )
    .unwrap();

    let mut source = read_test_task(root.path(), "T-source");
    source["review_followups"] = serde_json::json!([
        {
            "followup_id": "FUP-1",
            "status": "open",
            "description": "Create cleanup task"
        },
        {
            "followup_id": "FUP-2",
            "status": "done",
            "description": "Already handled"
        }
    ]);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-source.json"),
        serde_json::to_string_pretty(&source).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-pending");
    assert_eq!(payload["followup_source_count"], 1, "{payload}");
    assert_eq!(payload["followup_source_tasks"][0]["task_id"], "T-source");
    assert_eq!(
        payload["followup_source_tasks"][0]["followup_summary"]["open"],
        1
    );
    assert!(payload["message"]
        .as_str()
        .unwrap_or_default()
        .contains("open approved-review followups"));
}

#[tokio::test]
async fn test_ready_reconciles_started_blocked_task_back_to_in_progress_when_deps_clear() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-blocker", "closed", "task");
    write_test_task(root.path(), "T-dependent", "blocked", "task");

    let mut dependent = read_test_task(root.path(), "T-dependent");
    dependent["dependencies"] = serde_json::json!(["T-blocker"]);
    dependent["blocked_by"] = serde_json::json!(["T-blocker"]);
    dependent["assignee"] = Value::String("worker-1".to_string());
    dependent["percent"] = serde_json::json!(5);
    dependent["activity"] = Value::String("implementing".to_string());
    dependent["latest_commit"] = Value::String("abc123".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-dependent.json"),
        serde_json::to_string_pretty(&dependent).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 0, "{payload}");

    let updated = read_test_task(root.path(), "T-dependent");
    assert_eq!(updated["status"], "in_progress");
    assert_eq!(updated["assignee"], "worker-1");
    assert_eq!(updated["activity"], "implementing");
}

#[tokio::test]
async fn test_ready_reconciles_dependency_blocker_text_with_dead_assignee_back_to_pending() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-blocker", "closed", "task");
    write_test_task(root.path(), "T-dependent", "blocked", "task");

    let sessions_dir = root.path().join("runtime").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("worker-live.json"),
        serde_json::json!({
            "name": "worker-live",
            "role": "worker",
            "session_id": "sess-1",
            "registered_at": chrono::Utc::now().to_rfc3339(),
            "last_seen_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();

    let mut dependent = read_test_task(root.path(), "T-dependent");
    dependent["dependencies"] = serde_json::json!(["T-blocker"]);
    dependent["assignee"] = Value::String("dead-worker".to_string());
    dependent["percent"] = serde_json::json!(10);
    dependent["activity"] = Value::String("reading".to_string());
    dependent["latest_commit"] = Value::String("abc123".to_string());
    dependent["blockers"] = Value::String(
        "Imported dependency DAG is not yet satisfied: T-blocker is still InProgress. I am not starting code changes until that dependency completes."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-dependent.json"),
        serde_json::to_string_pretty(&dependent).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-dependent");

    let updated = read_test_task(root.path(), "T-dependent");
    assert_eq!(updated["status"], "pending");
    assert!(updated["assignee"].is_null(), "{updated:?}");
    assert_eq!(updated["orphaned_assignee"], "dead-worker");
    assert_eq!(updated["orphaned_status"], "in_progress");
    assert!(
        updated.get("blockers").is_none() || updated["blockers"].is_null(),
        "{updated:?}"
    );
}

#[tokio::test]
async fn test_ready_reconciles_worker_state_blocker_with_dead_assignee_back_to_pending() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    let sessions_dir = root.path().join("runtime").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("worker-live.json"),
        serde_json::json!({
            "name": "worker-live",
            "role": "worker",
            "session_id": "sess-1",
            "registered_at": chrono::Utc::now().to_rfc3339(),
            "last_seen_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();

    write_test_task(root.path(), "T-state-deadlock", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-state-deadlock");
    task["assignee"] = Value::String("dead-worker".to_string());
    task["percent"] = serde_json::json!(90);
    task["activity"] = Value::String("completing".to_string());
    task["latest_commit"] = Value::String("abc123".to_string());
    task["blockers"] = Value::String(
        "Brehon assignment mismatch during completion: complete call reports task assigned to '' rather than dead-worker, preventing worker completion handoff."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-state-deadlock.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 0, "{payload}");
    assert_eq!(payload["recoverable_blocked_count"], 1, "{payload}");
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["task_id"],
        "T-state-deadlock"
    );

    let repair = tool
        .execute(serde_json::json!({"action": "repair_frontier"}))
        .await
        .unwrap();
    let repair_payload: Value = serde_json::from_str(&extract_text(&repair)).unwrap();
    assert_eq!(repair_payload["repaired_count"], 1, "{repair_payload}");

    let updated = read_test_task(root.path(), "T-state-deadlock");
    assert_eq!(updated["status"], "review_ready");
    assert!(updated["assignee"].is_null(), "{updated:?}");
    assert!(
        updated.get("blockers").is_none() || updated["blockers"].is_null(),
        "{updated:?}"
    );
}

#[tokio::test]
async fn test_ready_reconciles_observed_worker_state_blocker_variants() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    let cases = [
        (
            "T-assignee-mismatch",
            "Unable to checkpoint/complete because Brehon reports task assignee mismatch: task is assigned to '' (empty) instead of worker-1, so worker permissions reject complete/checkpoint actions.",
        ),
        (
            "T-assigned-not",
            "Unable to submit completion: MCP returned \"Task T-x is assigned to '' not 'worker-2'. Only the assigned worker can checkpoint this task.\" Needs supervisor-side reassignment/ownership fix before worker can call complete.",
        ),
        (
            "T-ownership-drift",
            "Task ownership drift: completion rejected because assignee is empty string instead of worker-3, and mine now returns no assigned tasks despite local completed changes.",
        ),
        (
            "T-non-assignee",
            "Work is implemented locally but Brehon rejects completion from non-assignee.",
        ),
        (
            "T-not-permitted",
            "Cannot checkpoint/complete: MCP reports task assigned to worker-4, so worker-5 is not permitted to complete despite finished local changes and passing targeted tests.",
        ),
        (
            "T-pending-progress",
            "Task state is pending, so worker progress updates are rejected with invalid transition pending\u{2192}in_progress. Worker cannot set assigned status directly.",
        ),
    ];

    for (task_id, blockers) in cases {
        write_test_task(root.path(), task_id, "blocked", "task");
        let mut task = read_test_task(root.path(), task_id);
        task["assignee"] = Value::Null;
        task["review_owner"] = Value::Null;
        task["blockers"] = Value::String(blockers.to_string());
        task["percent"] = serde_json::json!(75);
        task["latest_commit"] = Value::String("abc123".to_string());
        std::fs::write(
            root.path()
                .join("runtime")
                .join("tasks")
                .join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
    }

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 0, "{payload}");
    assert_eq!(
        payload["recoverable_blocked_count"],
        cases.len(),
        "{payload}"
    );

    let repair = tool
        .execute(serde_json::json!({"action": "repair_frontier"}))
        .await
        .unwrap();
    let repair_payload: Value = serde_json::from_str(&extract_text(&repair)).unwrap();
    assert_eq!(
        repair_payload["repaired_count"],
        cases.len(),
        "{repair_payload}"
    );

    for (task_id, _) in cases {
        let updated = read_test_task(root.path(), task_id);
        assert_eq!(updated["status"], "review_ready", "{updated:?}");
        assert!(
            updated.get("blockers").is_none() || updated["blockers"].is_null(),
            "{updated:?}"
        );
    }
}

#[tokio::test]
async fn test_ready_recovers_dead_in_progress_assignee_without_worker_progress() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let sessions_dir = root.path().join("runtime").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("worker-live.json"),
        serde_json::json!({
            "name": "worker-live",
            "role": "worker",
            "session_id": "sess-1",
            "registered_at": chrono::Utc::now().to_rfc3339(),
            "last_seen_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();

    write_test_task(root.path(), "T-orphan-zero", "in_progress", "task");
    let mut task = read_test_task(root.path(), "T-orphan-zero");
    task["assignee"] = Value::String("dead-worker".to_string());
    task["percent"] = serde_json::json!(0);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-orphan-zero.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-orphan-zero");

    let updated = read_test_task(root.path(), "T-orphan-zero");
    assert_eq!(updated["status"], "pending");
    assert!(updated["assignee"].is_null(), "{updated:?}");
    assert_eq!(updated["orphaned_assignee"], "dead-worker");
    assert_eq!(updated["orphaned_status"], "in_progress");
}

#[tokio::test]
async fn test_ready_does_not_orphan_worker_with_unconsolidated_review_round() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let sessions_dir = root.path().join("runtime").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("worker-live.json"),
        serde_json::json!({
            "name": "worker-live",
            "role": "worker",
            "session_id": "sess-1",
            "registered_at": chrono::Utc::now().to_rfc3339(),
            "last_seen_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();

    write_test_task(root.path(), "T-review-active", "changes_requested", "task");
    let mut task = read_test_task(root.path(), "T-review-active");
    task["assignee"] = Value::String("dead-worker".to_string());
    task["review_owner"] = Value::String("dead-worker".to_string());
    task["activity"] = Value::String("awaiting review".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-review-active.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let round_dir = root
        .path()
        .join("runtime")
        .join("reviews")
        .join("T-review-active")
        .join("round-1");
    std::fs::create_dir_all(&round_dir).unwrap();
    std::fs::write(
        round_dir.join("request.json"),
        serde_json::json!({
            "task_id": "T-review-active",
            "review_id": "REV-active",
            "requested_by": "supervisor",
            "requested_at": chrono::Utc::now().to_rfc3339(),
            "title": "Active review",
            "description": "Review still collecting",
            "commit": "abc123",
            "base_commit": "base123",
            "merge_target_head": "base123",
            "commits": ["abc123"],
            "context": ""
        })
        .to_string(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    assert!(ready.is_error.is_none(), "{}", extract_text(&ready));

    let updated = read_test_task(root.path(), "T-review-active");
    assert_eq!(updated["status"], "changes_requested");
    assert_eq!(updated["assignee"], "dead-worker");
    assert_eq!(updated["review_owner"], "dead-worker");
    assert!(updated.get("orphaned_assignee").is_none(), "{updated:#?}");
}

#[tokio::test]
async fn test_ready_clears_stale_pending_assignee_without_worker_progress() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-pending", "pending", "task");
    let mut pending = read_test_task(root.path(), "T-pending");
    pending["assignee"] = Value::String("worker-1".to_string());
    pending["activity"] = Value::String("reviewing".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-pending.json"),
        serde_json::to_string_pretty(&pending).unwrap(),
    )
    .unwrap();

    tool.execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();

    let updated = read_test_task(root.path(), "T-pending");
    assert!(updated["assignee"].is_null(), "{updated:?}");
    assert!(updated.get("activity").is_none(), "{updated:?}");

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["count"], 1, "{payload}");
    assert_eq!(payload["tasks"][0]["task_id"], "T-pending");
}

#[tokio::test]
async fn test_children_defaults_to_compact_projection() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "E-1", "pending", "epic");
    write_test_task(root.path(), "T-1", "blocked", "task");

    let mut task = read_test_task(root.path(), "T-1");
    task["parent_id"] = Value::String("E-1".to_string());
    task["description"] =
        Value::String("Large body that should not be returned by default".to_string());
    task["notes"] = Value::String("Likewise should be omitted".to_string());
    task["blocked_by"] = serde_json::json!(["T-blocker"]);
    std::fs::write(
        root.path().join("runtime").join("tasks").join("T-1.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({"action": "children", "id": "E-1"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let child = &payload["children"][0];
    assert_eq!(payload["verbose"], false);
    assert_eq!(child["task_id"], "T-1");
    assert_eq!(child["status"], "blocked");
    assert_eq!(child["blocked_by"], serde_json::json!(["T-blocker"]));
    assert!(child.get("description").is_none(), "{payload}");
    assert!(child.get("notes").is_none(), "{payload}");
}

#[tokio::test]
async fn test_children_verbose_returns_full_task_payload() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "E-1", "pending", "epic");
    write_test_task(root.path(), "T-1", "pending", "task");

    let mut task = read_test_task(root.path(), "T-1");
    task["parent_id"] = Value::String("E-1".to_string());
    task["description"] =
        Value::String("Verbose output should preserve the full record".to_string());
    std::fs::write(
        root.path().join("runtime").join("tasks").join("T-1.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({"action": "children", "id": "E-1", "verbose": true}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let child = &payload["children"][0];
    assert_eq!(payload["verbose"], true);
    assert_eq!(child["task_id"], "T-1");
    assert_eq!(
        child["description"],
        "Verbose output should preserve the full record"
    );
}

#[tokio::test]
async fn test_archive_removes_task_and_strips_dependencies() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-blocker", "pending", "task");
    write_test_task(root.path(), "T-dependent", "blocked", "task");

    let mut dependent = read_test_task(root.path(), "T-dependent");
    dependent["dependencies"] = serde_json::json!(["T-blocker"]);
    dependent["blocked_by"] = serde_json::json!(["T-blocker"]);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-dependent.json"),
        serde_json::to_string_pretty(&dependent).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "archive",
            "id": "T-blocker",
            "role": "supervisor",
            "agent_name": "sup-1",
            "reason": "bogus blocker"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["action"], "archive");
    assert_eq!(payload["task_id"], "T-blocker");
    assert!(
        !root
            .path()
            .join("runtime")
            .join("tasks")
            .join("T-blocker.json")
            .exists(),
        "live task should be removed"
    );
    assert!(
        root.path()
            .join("runtime")
            .join("archive")
            .join("tasks")
            .join("T-blocker.json")
            .exists(),
        "archived task copy should exist"
    );
    let updated = read_test_task(root.path(), "T-dependent");
    assert_eq!(updated["status"], "pending");
    assert!(
        updated.get("dependencies").is_none() || updated["dependencies"] == serde_json::json!([]),
        "dependencies should be empty or omitted after archive cleanup: {updated:?}"
    );
}

#[tokio::test]
async fn test_archive_rejects_container_without_recursive() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "E-1", "pending", "epic");
    write_test_task(root.path(), "T-1", "pending", "task");
    let mut child = read_test_task(root.path(), "T-1");
    child["parent_id"] = Value::String("E-1".to_string());
    std::fs::write(
        root.path().join("runtime").join("tasks").join("T-1.json"),
        serde_json::to_string_pretty(&child).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "archive",
            "id": "E-1",
            "role": "supervisor",
            "agent_name": "sup-1",
            "reason": "bad epic"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_some());
    assert!(extract_text(&result).contains("recursive=true"));
    assert!(root
        .path()
        .join("runtime")
        .join("tasks")
        .join("E-1.json")
        .exists());
}

#[tokio::test]
async fn test_archive_rejects_active_review_obligation() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-review", "in_review", "task");
    write_review_metadata(root.path(), "T-review", "collecting", "deadbeef");

    let panels_dir = root.path().join("runtime").join("review-panels");
    std::fs::create_dir_all(&panels_dir).unwrap();
    let lease = serde_json::json!({
        "panel_id": "primary",
        "task_id": "T-review",
        "review_id": "REV-test",
        "round": 1,
        "members": [{
            "slot_agent": "claude-reviewer",
            "reviewer": "reviewer-1"
        }],
        "leased_at": "2026-04-09T00:00:00Z",
        "updated_at": "2026-04-09T00:00:00Z"
    });
    std::fs::write(
        panels_dir.join("primary.json"),
        serde_json::to_string_pretty(&lease).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "archive",
            "id": "T-review",
            "role": "supervisor",
            "agent_name": "sup-1",
            "reason": "remove broken review task"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_some());
    let text = extract_text(&result);
    assert!(
        text.contains("review obligations would be bypassed"),
        "{text}"
    );
    assert!(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-review.json")
            .exists(),
        "live task should remain"
    );
    assert!(
        root.path()
            .join("runtime")
            .join("review-panels")
            .join("primary.json")
            .exists(),
        "panel lease should remain"
    );
}

#[tokio::test]
async fn test_archive_rejects_checkpointed_unreviewed_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-checkpointed", "in_progress", "task");
    let mut task = read_test_task(root.path(), "T-checkpointed");
    task["latest_commit"] = Value::String("abc123".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-checkpointed.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "archive",
            "id": "T-checkpointed",
            "role": "supervisor",
            "agent_name": "sup-1",
            "reason": "remove unreviewed code"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_some());
    let text = extract_text(&result);
    assert!(text.contains("latest_commit"), "{text}");
    assert!(root
        .path()
        .join("runtime")
        .join("tasks")
        .join("T-checkpointed.json")
        .exists());
}

#[tokio::test]
async fn test_archive_allows_duplicate_checkpoint_already_owned_by_terminal_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-owner", "closed", "task");
    let mut owner = read_test_task(root.path(), "T-owner");
    owner["latest_commit"] = Value::String("abc123".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-owner.json"),
        serde_json::to_string_pretty(&owner).unwrap(),
    )
    .unwrap();

    write_test_task(root.path(), "T-duplicate", "blocked", "task");
    let mut duplicate = read_test_task(root.path(), "T-duplicate");
    duplicate["latest_commit"] = Value::String("abc123".to_string());
    duplicate["blockers"] = Value::String("Wrong repository task".to_string());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-duplicate.json"),
        serde_json::to_string_pretty(&duplicate).unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(
        root.path()
            .join("runtime")
            .join("reviews")
            .join("T-duplicate"),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "archive",
            "id": "T-duplicate",
            "role": "supervisor",
            "agent_name": "sup-1",
            "reason": "duplicate wrong-project task"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    assert!(root
        .path()
        .join("runtime")
        .join("tasks")
        .join("T-owner.json")
        .exists());
    assert!(!root
        .path()
        .join("runtime")
        .join("tasks")
        .join("T-duplicate.json")
        .exists());
    assert!(root
        .path()
        .join("runtime")
        .join("archive")
        .join("tasks")
        .read_dir()
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("T-duplicate")));
}

#[tokio::test]
async fn test_archive_terminal_task_moves_review_state_and_releases_panel() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-review", "merged", "task");
    write_review_metadata(root.path(), "T-review", "approved", "deadbeef");

    let panels_dir = root.path().join("runtime").join("review-panels");
    std::fs::create_dir_all(&panels_dir).unwrap();
    let lease = serde_json::json!({
        "panel_id": "primary",
        "task_id": "T-review",
        "review_id": "REV-test",
        "round": 1,
        "members": [{
            "slot_agent": "claude-reviewer",
            "reviewer": "reviewer-1"
        }],
        "leased_at": "2026-04-09T00:00:00Z",
        "updated_at": "2026-04-09T00:00:00Z"
    });
    std::fs::write(
        panels_dir.join("primary.json"),
        serde_json::to_string_pretty(&lease).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "archive",
            "id": "T-review",
            "role": "supervisor",
            "agent_name": "sup-1",
            "reason": "remove terminal reviewed task"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["released_panels"], serde_json::json!(["primary"]));
    assert!(
        !root
            .path()
            .join("runtime")
            .join("review-panels")
            .join("primary.json")
            .exists(),
        "panel lease should be removed"
    );
    assert!(
        !root
            .path()
            .join("runtime")
            .join("reviews")
            .join("T-review")
            .exists(),
        "live review dir should be removed"
    );
    assert!(
        root.path()
            .join("runtime")
            .join("archive")
            .join("reviews")
            .join("T-review")
            .exists(),
        "review state should be archived"
    );
}

#[tokio::test]
async fn test_archive_refreshes_runtime_stability_counters() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-review", "merged", "task");
    write_review_metadata(root.path(), "T-review", "collecting", "deadbeef");
    brehon_types::refresh_runtime_stability_counters(root.path()).unwrap();

    let before = brehon_types::load_runtime_stability_counters(
        &root.path().join("runtime").join("stability-counters.json"),
    )
    .unwrap();
    assert_eq!(before.completed_tasks, 1);
    assert_eq!(before.active_reviews, 1);

    let result = tool
        .execute(serde_json::json!({
            "action": "archive",
            "id": "T-review",
            "role": "supervisor",
            "agent_name": "sup-1",
            "reason": "remove archived task"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));

    let after = brehon_types::load_runtime_stability_counters(
        &root.path().join("runtime").join("stability-counters.json"),
    )
    .unwrap();
    assert_eq!(after.completed_tasks, 0);
    assert_eq!(after.active_reviews, 0);
}

#[tokio::test]
async fn test_container_close_rejects_archived_review_obligation_descendant() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "E-1", "pending", "epic");
    let archive_dir = root.path().join("runtime").join("archive").join("tasks");
    std::fs::create_dir_all(&archive_dir).unwrap();
    let archived = serde_json::json!({
        "task_id": "T-archived-review",
        "title": "Archived review bypass",
        "status": "review_ready",
        "task_type": "task",
        "parent_id": "E-1",
        "latest_commit": "abc123",
        "archived_at": "2026-05-02T00:00:00Z",
        "archive_reason": "reviewer broken"
    });
    std::fs::write(
        archive_dir.join("T-archived-review.json"),
        serde_json::to_string_pretty(&archived).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "E-1",
            "role": "supervisor",
            "agent_name": "sup-1"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_some());
    let text = extract_text(&result);
    assert!(
        text.contains("review obligations would be bypassed"),
        "{text}"
    );
    assert!(text.contains("T-archived-review"), "{text}");
    let epic = read_test_task(root.path(), "E-1");
    assert_eq!(epic["status"], "pending");
}

#[tokio::test]
async fn test_mine_hides_import_source_file_from_workers() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-1", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-1");
    task["assignee"] = Value::String("worker-1".to_string());
    task["plan_import"] = serde_json::json!({
        "source_file": "/Users/example/workspace/project/docs/project-plan.md",
        "source_task_id": "4.15"
    });
    std::fs::write(
        root.path().join("runtime").join("tasks").join("T-1.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let mine = tool
        .execute(serde_json::json!({"action": "mine"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&mine)).unwrap();
    let tasks = payload["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["plan_import"]["source_task_id"], "4.15");
    assert!(tasks[0]["plan_import"]["source_file"].is_null());
}

#[tokio::test]
async fn test_mine_only_returns_worker_actionable_assignments() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = TaskActionsTool::new();

    for (task_id, status) in [
        ("T-assigned", "assigned"),
        ("T-progress", "in_progress"),
        ("T-changes", "changes_requested"),
        ("T-ready", "review_ready"),
        ("T-review", "in_review"),
        ("T-approved", "approved"),
        ("T-blocked", "blocked"),
        ("T-merged", "merged"),
        ("T-closed", "closed"),
    ] {
        write_test_task(root.path(), task_id, status, "task");
    }

    let mine = tool
        .execute(serde_json::json!({"action": "mine"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&mine)).unwrap();
    let task_ids = payload["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|task| task["task_id"].as_str())
        .collect::<std::collections::HashSet<_>>();

    assert_eq!(payload["task_count"], 3);
    assert!(task_ids.contains("T-assigned"));
    assert!(task_ids.contains("T-progress"));
    assert!(task_ids.contains("T-changes"));
    assert!(!task_ids.contains("T-ready"));
    assert!(!task_ids.contains("T-review"));
    assert!(!task_ids.contains("T-approved"));
    assert!(!task_ids.contains("T-blocked"));
    assert!(!task_ids.contains("T-merged"));
    assert!(!task_ids.contains("T-closed"));
}

#[tokio::test]
async fn test_list_surfaces_assigned_without_delivery_observability() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-assigned", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-assigned");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": null,
        "delivery_method": "queued"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-assigned.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let list = tool
        .execute(serde_json::json!({
            "action": "list",
            "status": "assigned",
            "include_assignment_observability": true
        }))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&list)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["overall"], "assigned_without_delivery");
    assert_eq!(observability["delivery"]["state"], "not_enqueued");
}

#[tokio::test]
async fn test_list_omits_assignment_observability_by_default() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-assigned", "assigned", "task");
    let list = tool
        .execute(serde_json::json!({"action": "list", "status": "assigned"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&list)).unwrap();
    assert!(payload["tasks"][0]
        .get("assignment_observability")
        .is_none());
}

#[tokio::test]
async fn test_list_ignores_stale_propagation_owner_for_reassigned_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-reassigned-stale", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-reassigned-stale");
    task["assignee"] = serde_json::json!("worker-2");
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
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-reassigned-stale.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-worker-1", "worker-1", true);
    write_pane_assignment_context_fixture(
        root.path(),
        "worker-2",
        "task",
        "T-reassigned-stale",
        None,
        None,
    );

    let list = tool
        .execute(serde_json::json!({
            "action": "list",
            "status": "assigned",
            "include_assignment_observability": true
        }))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&list)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["owner"], "worker-2");
    assert_eq!(observability["overall"], "assigned_without_delivery");
    assert_eq!(observability["delivery"]["state"], "not_enqueued");
}

#[tokio::test]
async fn test_list_surfaces_delivered_without_ack_observability() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-delivered", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-delivered");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-delivered",
        "delivery_method": "queued"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-delivered.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-delivered", "worker-1", true);
    write_pane_assignment_context_fixture(
        root.path(),
        "worker-1",
        "task",
        "T-delivered",
        None,
        None,
    );

    let list = tool
        .execute(serde_json::json!({
            "action": "list",
            "status": "assigned",
            "include_assignment_observability": true
        }))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&list)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["overall"], "delivered_without_ack");
    assert_eq!(observability["delivery"]["state"], "injected");
    assert_eq!(observability["active_context"]["matches"], true);
    assert!(observability["acknowledged_at"].is_null());
}

#[tokio::test]
async fn test_mine_acknowledges_assignment_and_surfaces_acked_without_progress() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-acked", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-acked");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-acked",
        "delivery_method": "queued"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-acked.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-acked", "worker-1", true);
    write_pane_assignment_context_fixture(root.path(), "worker-1", "task", "T-acked", None, None);

    let mine = tool
        .execute(serde_json::json!({"action": "mine"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&mine)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["overall"], "acked_without_progress");
    assert_eq!(observability["acknowledged_by"], "worker-1");
    assert_eq!(observability["acknowledged_via"], "task action=mine");
    assert_eq!(observability["active_context"]["matches"], true);

    let stored = read_test_task(root.path(), "T-acked");
    assert_eq!(
        stored["assignment_propagation"]["acknowledged_via"],
        "task action=mine"
    );
}

#[tokio::test]
async fn test_mine_surfaces_persisted_not_enqueued_delivery_method() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-not-enqueued", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-not-enqueued");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-never-queued",
        "delivery_method": "persisted_not_enqueued",
        "acknowledged_at": "2026-05-24T01:00:10Z",
        "acknowledged_by": "worker-1",
        "acknowledged_via": "task action=mine"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-not-enqueued.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_pane_assignment_context_fixture(
        root.path(),
        "worker-1",
        "task",
        "T-not-enqueued",
        None,
        None,
    );

    let mine = tool
        .execute(serde_json::json!({"action": "mine"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&mine)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    // When the prompt was never enqueued, delivery is false so the overall
    // state is "assigned_without_delivery" even though the assignment is
    // acknowledged in the propagation.
    assert_eq!(observability["overall"], "assigned_without_delivery");
    let delivery = &observability["delivery"];
    assert_eq!(delivery["state"], "unknown");
    assert_eq!(delivery["enqueued"], false);
    assert_eq!(delivery["queued"], false);
    assert_eq!(delivery["injected"], false);
}

#[tokio::test]
async fn test_mine_observability_finds_sanitized_prompt_id_with_special_chars() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-sanitized-prompt", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-sanitized-prompt");
    let prompt_id = "prompt:review/1";
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": prompt_id,
        "delivery_method": "queued",
        "acknowledged_at": "2026-05-24T01:00:10Z",
        "acknowledged_by": "worker-1",
        "acknowledged_via": "task action=mine"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-sanitized-prompt.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), prompt_id, "worker-1", true);
    write_pane_assignment_context_fixture(
        root.path(),
        "worker-1",
        "task",
        "T-sanitized-prompt",
        None,
        None,
    );

    let mine = tool
        .execute(serde_json::json!({"action": "mine"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&mine)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["overall"], "acked_without_progress");
    let delivery = &observability["delivery"];
    assert_eq!(delivery["state"], "injected");
    assert_eq!(delivery["enqueued"], true);
    assert_eq!(delivery["injected"], true);
}

#[tokio::test]
async fn test_list_surfaces_acked_without_context_when_snapshot_mismatches() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-context-mismatch", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-context-mismatch");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-context-mismatch",
        "delivery_method": "queued",
        "acknowledged_at": "2026-05-24T01:00:10Z",
        "acknowledged_by": "worker-1",
        "acknowledged_via": "task action=mine"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-context-mismatch.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-context-mismatch", "worker-1", true);
    write_pane_assignment_context_fixture(root.path(), "worker-1", "task", "T-other", None, None);

    let list = tool
        .execute(serde_json::json!({
            "action": "list",
            "status": "assigned",
            "include_assignment_observability": true
        }))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&list)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["overall"], "acked_without_context");
    assert_eq!(observability["active_context"]["present"], true);
    assert_eq!(observability["active_context"]["matches"], false);
}

#[tokio::test]
async fn test_mine_does_not_treat_stale_reassigned_progress_as_current_assignment_progress() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-2"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-reassigned", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-reassigned");
    task["assignee"] = serde_json::json!("worker-2");
    task["percent"] = serde_json::json!(75);
    task["latest_commit"] = serde_json::json!("deadbeef");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-2",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-reassigned",
        "delivery_method": "queued"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-reassigned.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-reassigned", "worker-2", true);
    write_pane_assignment_context_fixture(
        root.path(),
        "worker-2",
        "task",
        "T-reassigned",
        None,
        None,
    );

    let mine = tool
        .execute(serde_json::json!({"action": "mine"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&mine)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["overall"], "acked_without_progress");
    assert_eq!(observability["progress_started"], false);
    assert!(observability["progress_started_at"].is_null());

    let stored = read_test_task(root.path(), "T-reassigned");
    assert!(
        stored["assignment_propagation"]["progress_started_at"].is_null(),
        "stale task-level percent/latest_commit must not count as progress for the new assignment"
    );
}

#[tokio::test]
async fn test_progress_acknowledges_assignment_without_prior_mine() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-progress-ack", "assigned", "task");
    let mut task = read_test_task(root.path(), "T-progress-ack");
    task["updated_at"] = Value::String("2026-05-24T00:59:00Z".to_string());
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-progress-ack",
        "delivery_method": "queued"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-progress-ack.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-progress-ack", "worker-1", true);
    write_pane_assignment_context_fixture(
        root.path(),
        "worker-1",
        "task",
        "T-progress-ack",
        None,
        None,
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-progress-ack",
            "percent": 35,
            "notes": "editing assignment",
            "activity": "editing"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));

    let stored = read_test_task(root.path(), "T-progress-ack");
    let updated_at = stored["updated_at"]
        .as_str()
        .expect("progress should bump updated_at");
    assert_ne!(updated_at, "2026-05-24T00:59:00Z");
    assert_eq!(
        stored["assignment_propagation"]["acknowledged_by"],
        "worker-1"
    );
    assert_eq!(
        stored["assignment_propagation"]["acknowledged_via"],
        "task action=progress"
    );
    assert_eq!(
        stored["assignment_propagation"]["progress_started_via"],
        "task action=progress"
    );

    let list = tool
        .execute(serde_json::json!({
            "action": "list",
            "status": "in_progress",
            "include_assignment_observability": true
        }))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&list)).unwrap();
    let observability = &payload["tasks"][0]["assignment_observability"];
    assert_eq!(observability["overall"], "active");
}

#[tokio::test]
async fn test_complete_acknowledges_assignment_without_prior_mine() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "completed\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-complete-ack", "in_progress", "task");
    let mut task = read_test_task(root.path(), "T-complete-ack");
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "assignment_kind": "task",
        "assigned_at": "2026-05-24T01:00:00Z",
        "prompt_id": "prompt-complete-ack",
        "delivery_method": "queued"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-complete-ack.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-complete-ack", "worker-1", true);
    write_pane_assignment_context_fixture(
        root.path(),
        "worker-1",
        "task",
        "T-complete-ack",
        None,
        None,
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-ack",
            "notes": "finished without prior mine"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));

    let stored = read_test_task(root.path(), "T-complete-ack");
    assert_eq!(
        stored["assignment_propagation"]["acknowledged_by"],
        "worker-1"
    );
    assert_eq!(
        stored["assignment_propagation"]["acknowledged_via"],
        "task action=progress"
    );
    assert_eq!(
        stored["assignment_propagation"]["progress_started_via"],
        "task action=progress"
    );
    assert!(stored["assignment_propagation"]["acknowledged_at"].is_string());
    assert!(stored["assignment_propagation"]["progress_started_at"].is_string());
}

#[tokio::test]
async fn test_mine_surfaces_review_obligations_even_when_role_env_is_lane_name() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "reviewer-b"),
        ("BREHON_AGENT_ROLE", "gemini-reviewer"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-review", "in_review", "task");
    let review_dir = root.path().join("runtime").join("reviews").join("T-review");
    let round_dir = review_dir.join("round-2");
    std::fs::create_dir_all(&round_dir).unwrap();
    std::fs::write(
        review_dir.join("state.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-review",
            "status": "collecting",
            "current_round": 2,
            "current_review_id": "REV-live",
            "max_rounds": 3,
            "panel_id": "primary",
            "panel_mode": "configured_panel",
            "panel": ["reviewer-a", "reviewer-b", "reviewer-c"],
            "submissions_received": ["reviewer-a"],
            "reviewer_assignments": {
                "reviewer-b": {
                    "owner": "reviewer-b",
                    "assignment_kind": "review",
                    "assigned_at": "2026-05-23T00:00:00Z",
                    "prompt_id": "prompt-review-b",
                    "delivery_method": "queued"
                }
            },
            "created_at": "2026-05-23T00:00:00Z",
            "updated_at": "2026-05-23T00:01:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        round_dir.join("request.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-review",
            "review_id": "REV-live",
            "requested_by": "supervisor-1",
            "requested_at": "2026-05-23T00:00:00Z",
            "title": "Reviewable task",
            "description": "review this",
            "commit": "abc123",
            "context": ""
        }))
        .unwrap(),
    )
    .unwrap();
    write_prompt_delivery_fixture(root.path(), "prompt-review-b", "reviewer-b", true);
    write_pane_assignment_context_fixture(
        root.path(),
        "reviewer-b",
        "review",
        "T-review",
        Some("REV-live"),
        Some(2),
    );

    let mine = tool
        .execute(serde_json::json!({"action": "mine"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&mine)).unwrap();

    assert_eq!(payload["task_count"], 0);
    assert_eq!(payload["review_count"], 1);
    assert_eq!(payload["count"], 1);
    assert_eq!(payload["has_assigned_work"], true);
    assert_eq!(
        payload["review_obligations"][0]["assignment_kind"],
        "review"
    );
    assert_eq!(payload["review_obligations"][0]["task_id"], "T-review");
    assert_eq!(payload["review_obligations"][0]["review_id"], "REV-live");
    assert_eq!(payload["review_obligations"][0]["reviewer"], "reviewer-b");
    assert_eq!(
        payload["review_obligations"][0]["next_action"]["args"]["action"],
        "submit_review"
    );
    assert_eq!(
        payload["review_obligations"][0]["assignment_observability"]["overall"],
        "acked_without_progress"
    );
    assert_eq!(
        payload["review_obligations"][0]["assignment_observability"]["acknowledged_via"],
        "task action=mine"
    );
    assert_eq!(payload["assignments"][0]["assignment_kind"], "review");

    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(review_dir.join("state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state["reviewer_assignments"]["reviewer-b"]["acknowledged_via"],
        "task action=mine"
    );
}

#[tokio::test]
async fn test_close_initiative_requires_child_epics_closed() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "I-1", "pending", "initiative");
    write_test_task(root.path(), "E-1", "pending", "epic");
    let mut epic = read_test_task(root.path(), "E-1");
    epic["parent_id"] = Value::String("I-1".to_string());
    std::fs::write(
        root.path().join("runtime").join("tasks").join("E-1.json"),
        serde_json::to_string_pretty(&epic).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "I-1",
            "role": "supervisor"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_some());
    assert!(extract_text(&result).contains("Cannot close initiative I-1"));
    assert!(extract_text(&result).contains("epics closed"));
}

#[tokio::test]
async fn test_create_infers_close_mode_for_audit_work() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": "Audit review lifecycle",
            "description": "Audit-only task with no code changes"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    if let ContentBlock::Text { text } = &result.content[0] {
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["completion_mode"], "close");
    }
}

#[tokio::test]
async fn test_create_rejects_thin_merge_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": "Implement login flow",
            "description": "Add the login flow."
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("Implementation merge tasks must include"));
    assert!(text.contains("acceptance_criteria"));
    assert!(text.contains("file_hints"));
}

#[tokio::test]
async fn test_create_rejects_control_plane_worker_task_scope() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": "Fix review panel config",
            "description": "Update the live Brehon review panel membership.",
            "completion_mode": "close",
            "acceptance_criteria": ["Panel members are corrected"],
            "file_hints": [".brehon/config.yaml"],
            "test_requirements": ["brehon config validate"],
            "plan_steps": ["Edit the live panel config"]
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("Concrete worker tasks cannot target live Brehon control-plane state"));
    assert!(text.contains(".brehon/"));
}

#[tokio::test]
async fn test_create_epic_requires_plan_detail() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "title": "Improve review flow",
            "description": "Coordinate the next wave of review hardening."
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("Epic tasks must include"));
    assert!(text.contains("acceptance_criteria"));
    assert!(text.contains("plan_steps or implementation_notes"));
}

#[tokio::test]
async fn test_create_epic_recovers_structured_sections_from_description() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let result = tool
            .execute(serde_json::json!({
                "action": "create",
                "task_type": "epic",
                "title": "Pane truthfulness enrichment",
                "description": "Add durable task/review truth to ACP panes.\n\nAcceptance Criteria:\n- Worker panes show real task state\n- Reviewer panes show real review state\n\nPlan:\n1. Extend runtime context readers\n2. Render the new sections in TUI\n\nImplementation Notes:\nReference real runtime JSON; do not infer state from chat text.",
                "direct_to_main": true
            }))
            .await
            .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let created: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(created["acceptance_criteria"].as_array().unwrap().len(), 2);
    assert_eq!(created["plan_steps"].as_array().unwrap().len(), 2);
    assert_eq!(
        created["implementation_notes"].as_str().unwrap(),
        "Reference real runtime JSON; do not infer state from chat text."
    );
    assert_eq!(
        created["description"].as_str().unwrap(),
        "Add durable task/review truth to ACP panes.\n\nAcceptance Criteria:\n- Worker panes show real task state\n- Reviewer panes show real review state\n\nPlan:\n- Extend runtime context readers\n- Render the new sections in TUI\n\nImplementation Notes:\nReference real runtime JSON; do not infer state from chat text."
    );
}

#[tokio::test]
async fn test_create_merge_task_recovers_structured_sections_from_description() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let result = tool
            .execute(serde_json::json!({
                "action": "create",
                "title": "Implement pane task header",
                "description": "Add a task header for worker panes.\n\nAcceptance Criteria:\n- Header shows task id and status\n\nFile Hints:\n- crates/brehon-tui/src/run.rs\n- crates/brehon-mux/src/pane/mod.rs\n\nTest Requirements:\n- cargo test -p brehon-tui\n- cargo test -p brehon-mux\n\nPlan:\n- Load task context into the pane renderer\n- Render a compact header above structured activity"
            }))
            .await
            .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let created: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(created["acceptance_criteria"].as_array().unwrap().len(), 1);
    assert_eq!(created["file_hints"].as_array().unwrap().len(), 2);
    assert_eq!(created["test_requirements"].as_array().unwrap().len(), 2);
    assert_eq!(created["plan_steps"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_create_prefers_top_level_structured_fields_over_description_sections() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();
    let result = tool
            .execute(serde_json::json!({
                "action": "create",
                "title": "Implement review summary widget",
                "description": "Render the widget.\n\nAcceptance Criteria:\n- stale description criterion\n\nFile Hints:\n- stale/file.rs\n\nTest Requirements:\n- cargo test stale\n\nPlan:\n- stale plan",
                "acceptance_criteria": ["Top-level acceptance"],
                "file_hints": ["crates/brehon-tui/src/run.rs"],
                "test_requirements": ["cargo test -p brehon-tui"],
                "plan_steps": ["Use top-level structure"],
                "implementation_notes": "Prefer explicit args over parsed prose."
            }))
            .await
            .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let created: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(
        created["acceptance_criteria"].as_array().unwrap()[0]
            .as_str()
            .unwrap(),
        "Top-level acceptance"
    );
    assert_eq!(
        created["file_hints"].as_array().unwrap()[0]
            .as_str()
            .unwrap(),
        "crates/brehon-tui/src/run.rs"
    );
    assert_eq!(
        created["test_requirements"].as_array().unwrap()[0]
            .as_str()
            .unwrap(),
        "cargo test -p brehon-tui"
    );
    assert_eq!(
        created["plan_steps"].as_array().unwrap()[0]
            .as_str()
            .unwrap(),
        "Use top-level structure"
    );
    assert_eq!(
        created["implementation_notes"].as_str().unwrap(),
        "Prefer explicit args over parsed prose."
    );
    assert!(!created["description"]
        .as_str()
        .unwrap()
        .contains("stale description criterion"));
}

#[tokio::test]
async fn test_create_missing_title() {
    let tool = TaskActionsTool::new();
    let args = serde_json::json!({ "action": "create" });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_unknown_action() {
    let tool = TaskActionsTool::new();
    let args = serde_json::json!({ "action": "bogus" });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

// ── State machine validation tests ──────────────────────────────

#[test]
fn test_valid_lifecycle_transitions() {
    // Full happy path: pending → assigned → in_progress → in_review → approved → merged
    assert!(validate_status_transition(
        "pending",
        "assigned",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "assigned",
        "in_progress",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "in_progress",
        "in_review",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "in_review",
        "approved",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "approved",
        "merged",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
}

#[test]
fn test_changes_requested_cycle() {
    assert!(validate_status_transition(
        "in_review",
        "changes_requested",
        "reviewer",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "changes_requested",
        "in_progress",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
}

#[test]
fn test_block_unblock_cycle() {
    assert!(validate_status_transition(
        "in_progress",
        "blocked",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "blocked",
        "pending",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
}

#[test]
fn test_same_status_is_noop() {
    assert!(validate_status_transition(
        "in_progress",
        "in_progress",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "pending",
        "pending",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
}

#[test]
fn test_worker_cannot_set_approved() {
    let err = validate_status_transition(
        "in_review",
        "approved",
        "worker",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Workers cannot set status to 'approved'"));
}

#[test]
fn test_worker_cannot_skip_to_merged() {
    let err = validate_status_transition(
        "in_progress",
        "merged",
        "worker",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Only supervisors can set status to 'merged'"));
}

#[test]
fn test_cannot_skip_review() {
    // in_progress → approved (skipping in_review)
    let err = validate_status_transition(
        "in_progress",
        "approved",
        "supervisor",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Invalid status transition"));
}

#[test]
fn test_cannot_regress_from_approved() {
    // approved → in_progress (going backwards)
    let err = validate_status_transition(
        "approved",
        "in_progress",
        "supervisor",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Invalid status transition"));
}

#[test]
fn test_terminal_states_reject_all() {
    let err = validate_status_transition(
        "merged",
        "pending",
        "supervisor",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("terminal state"));

    let err = validate_status_transition(
        "closed",
        "pending",
        "supervisor",
        "task",
        TaskCompletionMode::Close,
    )
    .unwrap_err();
    assert!(err.contains("terminal state"));
}

#[test]
fn test_pending_cannot_jump_to_in_progress() {
    // Must go through assigned first
    let err = validate_status_transition(
        "pending",
        "in_progress",
        "worker",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Invalid status transition"));
}

#[test]
fn test_close_mode_tasks_can_close_from_approved() {
    assert!(validate_status_transition(
        "approved",
        "closed",
        "supervisor",
        "task",
        TaskCompletionMode::Close
    )
    .is_ok());
}

#[test]
fn test_merge_mode_tasks_cannot_close_from_approved() {
    let err = validate_status_transition(
        "approved",
        "closed",
        "supervisor",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Invalid status transition"));
}

#[test]
fn test_supervisor_can_reopen_approved_task_for_revision() {
    assert!(validate_status_transition(
        "approved",
        "changes_requested",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
}

#[test]
fn test_worker_cannot_reopen_approved_task_for_revision() {
    let err = validate_status_transition(
        "approved",
        "changes_requested",
        "worker",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Only supervisors can reopen an approved task"));
}

#[test]
fn test_epic_bypasses_state_machine() {
    // Epics have simpler lifecycle
    assert!(validate_status_transition(
        "pending",
        "in_progress",
        "supervisor",
        "epic",
        TaskCompletionMode::Close
    )
    .is_ok());
    assert!(validate_status_transition(
        "pending",
        "closed",
        "supervisor",
        "epic",
        TaskCompletionMode::Close
    )
    .is_ok());
}

#[test]
fn test_reassign_transitions() {
    assert!(validate_status_transition(
        "assigned",
        "pending",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "in_progress",
        "pending",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "in_review",
        "pending",
        "supervisor",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
}

#[test]
fn test_pascal_case_statuses_are_normalized() {
    assert!(validate_status_transition(
        "Assigned",
        "InProgress",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
    assert!(validate_status_transition(
        "ChangesRequested",
        "InProgress",
        "worker",
        "task",
        TaskCompletionMode::Merge
    )
    .is_ok());
}

#[test]
fn test_unknown_statuses_are_rejected() {
    let err = validate_status_transition(
        "mystery",
        "in_progress",
        "worker",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Unknown current task status"));

    let err = validate_status_transition(
        "assigned",
        "mystery",
        "worker",
        "task",
        TaskCompletionMode::Merge,
    )
    .unwrap_err();
    assert!(err.contains("Unknown proposed task status"));
}

#[tokio::test]
async fn test_progress_rejects_review_and_terminal_states() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();

    for status in ["in_review", "approved", "merged"] {
        let task_id = format!("T-{}", status);
        write_test_task(root.path(), &task_id, status, "task");

        let result = tool
            .execute(serde_json::json!({
                "action": "progress",
                "id": task_id,
                "percent": 50
            }))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true), "status={status}");
        let text = extract_text(&result);
        assert!(
            text.contains("Invalid status transition") || text.contains("terminal state"),
            "unexpected error for {status}: {text}"
        );

        let task = read_test_task(root.path(), &format!("T-{}", status));
        assert_eq!(task["status"], status, "status mutated for {status}");
    }
}

#[tokio::test]
async fn test_progress_rejects_non_assignee_worker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-2"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-owned-progress", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-owned-progress",
            "percent": 50,
            "notes": "still working"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("assigned to 'worker-1' not 'worker-2'"),
        "{text}"
    );
    assert!(text.contains("Only the assigned worker can report progress"));

    let task = read_test_task(root.path(), "T-owned-progress");
    assert_eq!(task["status"], "in_progress");
    assert_eq!(task["percent"], 0);
    assert!(task.get("notes").is_none());
}

#[tokio::test]
async fn test_progress_rejects_unassigned_changes_requested_self_claim() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(
        root.path(),
        "T-unassigned-revision",
        "changes_requested",
        "task",
    );
    let mut task = read_test_task(root.path(), "T-unassigned-revision");
    task["assignee"] = Value::Null;
    task["review_owner"] = Value::Null;
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-unassigned-revision.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-unassigned-revision",
            "percent": 50,
            "notes": "continuing revision"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("assigned to '' not 'worker-1'"), "{text}");
    assert!(
        text.contains("Workers cannot claim active or revision tasks via `task action=progress`"),
        "{text}"
    );

    let stored = read_test_task(root.path(), "T-unassigned-revision");
    assert_eq!(stored["status"], "changes_requested");
    assert_eq!(stored["assignee"], Value::Null);
    assert_eq!(stored["percent"], 0);
    assert!(stored.get("notes").is_none());
}

#[tokio::test]
async fn test_progress_rejects_worker_with_duplicate_active_assignment() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-current", "changes_requested", "task");
    write_test_task(root.path(), "T-other", "changes_requested", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-current",
            "percent": 50,
            "notes": "continuing current task"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("worker 'worker-1' already owns active task(s): T-other [changes_requested]"),
        "{text}"
    );
    assert!(text.contains("Workers are single-task"), "{text}");

    let stored = read_test_task(root.path(), "T-current");
    assert_eq!(stored["status"], "changes_requested");
    assert_eq!(stored["percent"], 0);
    assert!(stored.get("notes").is_none());
}

#[tokio::test]
async fn test_progress_100_from_changes_requested_auto_marks_review_ready() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-reround", "changes_requested", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-reround",
            "percent": 100,
            "notes": "reround fixes complete",
            "activity": "testing"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["auto_review"], true);
    assert_eq!(result_json["task_status"], "review_ready");

    let task = read_test_task(root.path(), "T-reround");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["percent"], 100);
    assert_eq!(task["notes"], "reround fixes complete");
    assert_eq!(task["assignee"], "worker-1");
    assert_eq!(task["review_owner"], "worker-1");

    let notified = read_queued_prompts(root.path()).into_iter().any(|payload| {
        if payload["target"] != "sup-1" {
            return false;
        }
        let message = payload["message"].as_str().unwrap_or("");
        message.contains("Task T-reround")
            && message.contains("ready for review")
            && message.contains("review_ready")
    });
    assert!(
        notified,
        "supervisor should be notified that reround work is ready for review"
    );
}

#[tokio::test]
async fn test_progress_100_on_review_ready_is_idempotent() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-already-ready", "review_ready", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-already-ready",
            "percent": 100,
            "notes": "still complete",
            "activity": "testing"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["task_status"], "review_ready");
    assert_eq!(result_json["already_handed_off"], true);
    assert_eq!(result_json["auto_review"], true);

    let task = read_test_task(root.path(), "T-already-ready");
    assert_eq!(task["status"], "review_ready");
}

#[tokio::test]
async fn test_progress_100_from_changes_requested_clears_obsolete_review_state() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-reround-state", "changes_requested", "task");
    write_review_metadata(
        root.path(),
        "T-reround-state",
        "changes_requested",
        "deadbeef",
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-reround-state",
            "percent": 100,
            "notes": "reround fixes complete",
            "activity": "testing"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    assert!(
        !root
            .path()
            .join("runtime")
            .join("reviews")
            .join("T-reround-state")
            .join("state.json")
            .exists(),
        "obsolete changes_requested review state should be cleared once work is ready for rereview"
    );
}

#[tokio::test]
async fn test_progress_100_resolves_live_supervisor_session_when_env_missing() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", ""),
    ]);
    crate::tools::agent::write_session_file(
        "claude-code",
        "supervisor",
        "sup-session",
        Some("claude"),
    );
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-live-supervisor", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-live-supervisor",
            "percent": 100,
            "notes": "implementation complete",
            "activity": "testing"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["auto_review"], true);
    assert!(result_json.get("warning").is_none());

    let task = read_test_task(root.path(), "T-live-supervisor");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["assignee"], "worker-1");
    assert_eq!(task["review_owner"], "worker-1");

    let notified = read_queued_prompts(root.path())
        .into_iter()
        .any(|payload| payload["target"] == "claude-code");
    assert!(
        notified,
        "worker completion should resolve and notify the live supervisor session"
    );
}

#[tokio::test]
async fn test_progress_100_records_worker_commit_from_workspace_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    let head_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-worker-commit", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-worker-commit",
            "percent": 100,
            "notes": "implementation complete",
            "activity": "testing"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["latest_commit"], head_commit);

    let task = read_test_task(root.path(), "T-worker-commit");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["assignee"], "worker-1");
    assert_eq!(task["review_owner"], "worker-1");
    assert_eq!(task["latest_commit"], head_commit);

    let saw_commit = read_queued_prompts(root.path()).into_iter().any(|payload| {
        payload["target"] == "sup-1"
            && payload["message"]
                .as_str()
                .unwrap_or("")
                .contains(&head_commit)
    });
    assert!(
        saw_commit,
        "supervisor notification should include the worker commit"
    );
}

#[tokio::test]
async fn test_checkpoint_commits_worker_workspace_and_records_latest_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    let initial_commit = init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "checkpointed\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-checkpoint", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "checkpoint",
            "id": "T-checkpoint",
            "message": "checkpoint worker changes"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let commit = result_json["latest_commit"].as_str().unwrap();
    assert_ne!(commit, initial_commit);
    assert_eq!(result_json["created_commit"], true);
    assert_eq!(run_git(workspace.path(), &["rev-parse", "HEAD"]), commit);
    assert_eq!(run_git(workspace.path(), &["status", "--porcelain"]), "");

    let task = read_test_task(root.path(), "T-checkpoint");
    assert_eq!(task["latest_commit"], commit);
}

#[tokio::test]
async fn test_checkpoint_rejects_hallucinated_completion_message() {
    // Regression guard: workers sometimes called `action=checkpoint` with a
    // message asserting the task was complete / in review, then went idle
    // waiting for a handoff that only `action=complete` performs. Before
    // this guard the MCP silently accepted the checkpoint and the task got
    // stuck in `in_progress` until the 15-minute idle-recycle fired. Now
    // the checkpoint is rejected at the boundary with an explicit
    // redirect to `action=complete`.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    let _initial_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-halluc", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "checkpoint",
            "id": "T-halluc",
            "message": "All follow-up fixes done, tests pass, task is now in review."
        }))
        .await
        .unwrap();

    let text = extract_text(&result);
    assert!(
        result.is_error.unwrap_or(false),
        "expected error_result for hallucinated checkpoint; got: {text}"
    );
    assert!(
        text.contains("task is now in review"),
        "rejection should quote the matched phrase; got: {text}"
    );
    assert!(
        text.contains("action=complete"),
        "rejection should redirect to action=complete; got: {text}"
    );

    // Task must NOT have been mutated — no checkpoint commit recorded.
    let task = read_test_task(root.path(), "T-halluc");
    assert!(
        task.get("latest_commit").is_none(),
        "rejected checkpoint should not persist latest_commit; task: {task}"
    );
}

#[tokio::test]
async fn test_checkpoint_accepts_neutral_mid_task_message() {
    // Positive control: a neutral checkpoint message with factual
    // test-status reporting but no completion-implying prose must still
    // succeed. This is the common legitimate case (mid-task snapshot,
    // tests passing right now, more work to do).
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    let _initial_commit = init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("wip.txt"), "wip\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-neutral", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "checkpoint",
            "id": "T-neutral",
            "message": "9 tc-nas5gs tests pass; starting next fix"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "neutral checkpoint message was wrongly rejected: {}",
        extract_text(&result)
    );
}

#[tokio::test]
async fn test_complete_commits_worker_workspace_and_marks_review_ready() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    let initial_commit = init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "completed\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-complete", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete",
            "notes": "implementation complete"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let commit = result_json["latest_commit"].as_str().unwrap();
    assert_ne!(commit, initial_commit);
    assert_eq!(result_json["created_commit"], true);
    assert_eq!(result_json["task_status"], "review_ready");
    assert_eq!(run_git(workspace.path(), &["rev-parse", "HEAD"]), commit);
    assert_eq!(run_git(workspace.path(), &["status", "--porcelain"]), "");

    let task = read_test_task(root.path(), "T-complete");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["notes"], "implementation complete");
    assert_eq!(task["latest_commit"], commit);
    assert_eq!(task["review_owner"], "worker-1");

    let notified = read_queued_prompts(root.path()).into_iter().any(|payload| {
        payload["target"] == "sup-1"
            && payload["message"]
                .as_str()
                .unwrap_or("")
                .contains("Task T-complete")
    });
    assert!(
        notified,
        "supervisor should be notified when complete succeeds"
    );
}

#[tokio::test]
async fn test_complete_recovers_blocked_handoff_after_checkpoint_succeeds() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    let initial_commit = init_git_workspace(workspace.path());
    std::fs::write(
        workspace.path().join("feature.txt"),
        "completed from blocked\n",
    )
    .unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-complete-blocked", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-complete-blocked");
    task["assignee"] = Value::String("worker-1".to_string());
    task["blockers"] = Value::String(
        "State deadlock: checkpoint created during pending state, need reassignment to complete"
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-complete-blocked.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-blocked",
            "notes": "implementation complete after blocked handoff"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let commit = result_json["latest_commit"].as_str().unwrap();
    assert_ne!(commit, initial_commit);
    assert_eq!(result_json["created_commit"], true);
    assert_eq!(result_json["task_status"], "review_ready");
    assert_eq!(result_json["recovered_handoff"], true);
    assert_eq!(run_git(workspace.path(), &["rev-parse", "HEAD"]), commit);
    assert_eq!(run_git(workspace.path(), &["status", "--porcelain"]), "");

    let task = read_test_task(root.path(), "T-complete-blocked");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["latest_commit"], commit);
    assert!(task.get("blockers").is_none(), "blockers should be cleared");
    assert!(task["assignee"].is_null());
    assert!(task["review_owner"].is_null());
    assert_eq!(task["activity"], "awaiting_review");
    assert_eq!(task["percent"], 100);
}

#[tokio::test]
async fn test_complete_recovers_legacy_completed_handoff_after_checkpoint_succeeds() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    let initial_commit = init_git_workspace(workspace.path());
    std::fs::write(
        workspace.path().join("feature.txt"),
        "completed from legacy state\n",
    )
    .unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(
        root.path(),
        "T-complete-legacy-completed",
        "completed",
        "task",
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-legacy-completed",
            "notes": "implementation complete after legacy completed state"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let commit = result_json["latest_commit"].as_str().unwrap();
    assert_ne!(commit, initial_commit);
    assert_eq!(result_json["created_commit"], true);
    assert_eq!(result_json["task_status"], "review_ready");
    assert_eq!(result_json["recovered_handoff"], true);
    assert_eq!(run_git(workspace.path(), &["rev-parse", "HEAD"]), commit);
    assert_eq!(run_git(workspace.path(), &["status", "--porcelain"]), "");

    let task = read_test_task(root.path(), "T-complete-legacy-completed");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["latest_commit"], commit);
    assert!(task["assignee"].is_null());
    assert!(task["review_owner"].is_null());
    assert_eq!(task["activity"], "awaiting_review");
    assert_eq!(task["percent"], 100);
}

#[tokio::test]
async fn test_close_rejects_dirty_shared_root_before_terminal_status() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    run_git(project.path(), &["checkout", "main"]);
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", project.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task_with_mode(
        root.path(),
        "T-dirty-terminal-close",
        "approved",
        "task",
        "close",
        "close-mode task",
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-dirty-terminal-close"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("shared repo root"), "{text}");
    assert!(text.contains("Refusing terminal transition"), "{text}");
    assert!(text.contains("leaked.txt"), "{text}");
    assert!(text.contains("Recovery:"), "{text}");

    let task = read_test_task(root.path(), "T-dirty-terminal-close");
    assert_eq!(task["status"], "approved");
}

#[tokio::test]
async fn test_update_rejects_terminal_status_when_shared_root_is_dirty() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    run_git(project.path(), &["checkout", "main"]);
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", project.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task_with_mode(
        root.path(),
        "T-dirty-terminal-update",
        "approved",
        "task",
        "close",
        "close-mode task",
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-dirty-terminal-update",
            "status": "closed"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("shared repo root"), "{text}");
    assert!(text.contains("Refusing terminal transition"), "{text}");
    assert!(text.contains("leaked.txt"), "{text}");
    assert!(text.contains("Recovery:"), "{text}");

    let task = read_test_task(root.path(), "T-dirty-terminal-update");
    assert_eq!(task["status"], "approved");
}

#[tokio::test]
async fn test_close_epic_rejects_dirty_shared_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    run_git(project.path(), &["checkout", "main"]);
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", project.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "E-dirty-epic-close", "approved", "epic");

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "E-dirty-epic-close"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("shared repo root"), "{text}");
    assert!(text.contains("leaked.txt"), "{text}");

    let task = read_test_task(root.path(), "E-dirty-epic-close");
    assert_eq!(task["status"], "approved");
}

#[tokio::test]
async fn test_close_initiative_rejects_dirty_shared_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    run_git(project.path(), &["checkout", "main"]);
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", project.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(
        root.path(),
        "I-dirty-initiative-close",
        "approved",
        "initiative",
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "I-dirty-initiative-close"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("shared repo root"), "{text}");
    assert!(text.contains("leaked.txt"), "{text}");

    let task = read_test_task(root.path(), "I-dirty-initiative-close");
    assert_eq!(task["status"], "approved");
}

#[tokio::test]
async fn test_complete_rejects_empty_worker_handoff() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());

    let workspace = tempfile::tempdir().unwrap();
    let initial_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-empty-handoff", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-empty-handoff",
            "notes": "implementation complete"
        }))
        .await
        .unwrap();

    let text = extract_text(&result);
    assert!(
        result.is_error.unwrap_or(false),
        "expected empty-handoff rejection, got: {text}"
    );
    assert!(
        text.contains("Refusing empty checkpoint"),
        "rejection should explain empty checkpoint safety; got: {text}"
    );
    assert_eq!(
        run_git(workspace.path(), &["rev-parse", "HEAD"]),
        initial_commit
    );

    let task = read_test_task(root.path(), "T-empty-handoff");
    assert_eq!(task["status"], "in_progress");
    assert!(task.get("latest_commit").is_none());
    assert!(task.get("checkpoint_warnings").is_none());
}

#[tokio::test]
async fn test_complete_rejects_dirty_shared_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "done\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature done"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-dirty-complete", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-dirty-complete",
            "notes": "implementation complete"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("shared repo root"), "{text}");
    assert!(text.contains("leaked.txt"), "{text}");
    assert!(text.contains("Recovery:"), "{text}");

    let task = read_test_task(root.path(), "T-dirty-complete");
    assert_eq!(task["status"], "in_progress");
}

#[tokio::test]
async fn test_update_task_status_atomic_rejects_dirty_shared_root_on_approval() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", project.path().to_str().unwrap()),
    ]);

    let tasks_dir = root.path().join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("T-dirty-approve.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-dirty-approve",
            "title": "Dirty approve fixture",
            "description": "Fixture",
            "status": "in_review",
            "task_type": "task",
            "completion_mode": "merge",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();

    let err = update_task_status_atomic("T-dirty-approve", "approved")
        .await
        .unwrap_err();
    assert!(err.contains("shared repo root"), "{err}");
    assert!(err.contains("leaked.txt"), "{err}");
    assert!(err.contains("Recovery:"), "{err}");

    let task = read_test_task(root.path(), "T-dirty-approve");
    assert_eq!(task["status"], "in_review");
}

#[tokio::test]
async fn test_update_task_status_atomic_allows_changes_requested_on_dirty_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", project.path().to_str().unwrap()),
    ]);

    let tasks_dir = root.path().join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("T-dirty-changes.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-dirty-changes",
            "title": "Dirty changes_requested fixture",
            "description": "Fixture",
            "status": "in_review",
            "task_type": "task",
            "completion_mode": "merge",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();

    // changes_requested should NOT be blocked by dirty root (only approved is)
    update_task_status_atomic("T-dirty-changes", "changes_requested")
        .await
        .expect("changes_requested should succeed even when shared root is dirty");

    let task = read_test_task(root.path(), "T-dirty-changes");
    assert_eq!(task["status"], "changes_requested");
}

#[tokio::test]
async fn test_dirty_root_blocker_includes_current_runtime_session_when_known() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    std::fs::write(project.path().join("leaked.txt"), "wrong tree\n").unwrap();

    // Create current-session.json so current runtime session is known
    let runtime_dir = root.path().join("runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::write(
        runtime_dir.join("current-session.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "session_name": "test-run-42"
        }))
        .unwrap(),
    )
    .unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", project.path().to_str().unwrap()),
    ]);

    let tasks_dir = root.path().join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("T-dirty-runtime-session.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-dirty-runtime-session",
            "title": "Current runtime session fixture",
            "description": "Fixture",
            "status": "in_review",
            "task_type": "task",
            "completion_mode": "merge",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();

    let err = update_task_status_atomic("T-dirty-runtime-session", "approved")
        .await
        .unwrap_err();
    assert!(err.contains("shared repo root"), "{err}");
    assert!(
        err.contains("Current runtime session: test-run-42."),
        "{err}"
    );
    assert!(err.contains("Recovery:"), "{err}");

    let task = read_test_task(root.path(), "T-dirty-runtime-session");
    assert_eq!(task["status"], "in_review");
}

#[tokio::test]
async fn test_complete_allows_completion_language_in_notes() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "completed\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-complete-phrase", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-phrase",
            "notes": "Task complete and ready for review."
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["task_status"], "review_ready");

    let task = read_test_task(root.path(), "T-complete-phrase");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["notes"], "Task complete and ready for review.");
}

#[tokio::test]
async fn test_complete_recovers_started_pending_task_for_same_worker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let project = tempfile::tempdir().unwrap();
    init_git_workspace(project.path());
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "completed\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-pending-complete", "pending", "task");
    let mut task = read_test_task(root.path(), "T-pending-complete");
    task["percent"] = serde_json::json!(25);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-pending-complete.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-pending-complete",
            "notes": "implementation complete"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["task_status"], "review_ready");

    let task = read_test_task(root.path(), "T-pending-complete");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["review_owner"], "worker-1");
    assert_eq!(task["percent"], 100);
}

#[tokio::test]
async fn test_complete_is_idempotent_when_task_already_review_ready() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    let initial_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-complete-ready", "review_ready", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-ready",
            "notes": "already ready"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["task_status"], "review_ready");
    assert_eq!(result_json["already_handed_off"], true);
    assert_eq!(result_json["latest_commit"], initial_commit);
    assert_eq!(result_json["created_commit"], false);

    let task = read_test_task(root.path(), "T-complete-ready");
    assert_eq!(task["status"], "review_ready");
}

#[tokio::test]
async fn test_complete_is_idempotent_when_task_already_in_review() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    let initial_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-complete-reviewing", "in_review", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-reviewing",
            "notes": "review already started"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["task_status"], "in_review");
    assert_eq!(result_json["already_handed_off"], true);
    assert_eq!(result_json["latest_commit"], initial_commit);
    assert_eq!(result_json["created_commit"], false);

    let task = read_test_task(root.path(), "T-complete-reviewing");
    assert_eq!(task["status"], "in_review");
}

#[tokio::test]
async fn test_complete_defaults_to_existing_worker_commit_and_default_notes() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("manual.txt"), "manual commit\n").unwrap();
    run_git(workspace.path(), &["add", "manual.txt"]);
    run_git(workspace.path(), &["commit", "-m", "manual worker commit"]);
    let existing_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-complete-defaults", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-defaults"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["created_commit"], false);
    assert_eq!(result_json["latest_commit"], existing_commit);
    assert_eq!(result_json["task_status"], "review_ready");

    let task = read_test_task(root.path(), "T-complete-defaults");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["notes"], "Implementation complete");
    assert_eq!(task["latest_commit"], existing_commit);
}

#[tokio::test]
async fn test_complete_rejects_non_worker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-complete-role", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "complete",
            "id": "T-complete-role"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("Only workers can complete tasks."));
}

#[tokio::test]
async fn test_checkpoint_rejects_non_assignee() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "checkpointed\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-2"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-checkpoint-owner", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "checkpoint",
            "id": "T-checkpoint-owner",
            "message": "checkpoint worker changes"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("Only the assigned worker can checkpoint this task."));
}

#[tokio::test]
async fn test_progress_rejects_worker_on_merge_target_branch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_WORKTREE_BRANCH", "brehon/worker-1"),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-wrong-branch", "in_progress", "task");

    let mut task = read_test_task(root.path(), "T-wrong-branch");
    task["merge_target"] = "epic/test-feature".into();
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-wrong-branch.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-wrong-branch",
            "percent": 50,
            "notes": "editing on wrong branch",
            "activity": "editing"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("Worker branch mismatch") || text.contains("merge_target"),
        "unexpected error: {text}"
    );

    let stored = read_test_task(root.path(), "T-wrong-branch");
    assert_eq!(stored["status"], "in_progress");
    assert_eq!(stored["percent"], 0);
}

#[tokio::test]
async fn test_progress_rejects_control_plane_task_scope() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-control-plane", "in_progress", "task");
    let mut task = read_test_task(root.path(), "T-control-plane");
    task["file_hints"] = serde_json::json!([".brehon/runtime/reviews/T-1/state.json"]);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-control-plane.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-control-plane",
            "percent": 50
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("targets live Brehon control-plane state"));
}

#[tokio::test]
async fn test_checkpoint_rejects_control_plane_task_scope() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "checkpointed\n").unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-control-checkpoint", "in_progress", "task");
    let mut task = read_test_task(root.path(), "T-control-checkpoint");
    task["file_hints"] = serde_json::json!([".brehon/config.toml"]);
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-control-checkpoint.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "checkpoint",
            "id": "T-control-checkpoint",
            "message": "should fail"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("targets live Brehon control-plane state"));
}

#[tokio::test]
async fn test_progress_100_warns_when_no_supervisor_can_be_resolved() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", ""),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-no-supervisor", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "progress",
            "id": "T-no-supervisor",
            "percent": 100,
            "notes": "implementation complete",
            "activity": "testing"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["auto_review"], true);
    assert_eq!(result_json["task_status"], "review_ready");
    assert!(result_json["warning"]
        .as_str()
        .unwrap_or("")
        .contains("no live supervisor session could be resolved"));

    let task = read_test_task(root.path(), "T-no-supervisor");
    assert_eq!(task["status"], "review_ready");
    assert_eq!(task["assignee"], "worker-1");
    assert_eq!(task["review_owner"], "worker-1");

    let queued = read_queued_prompts(root.path());
    assert_eq!(
        queued.len(),
        0,
        "must not queue to a fake supervisor target"
    );
}

#[tokio::test]
async fn test_close_rejects_worker_merge_of_approved_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-approved-close", "approved", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-approved-close"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("Only supervisors can close tasks"));

    let task = read_test_task(root.path(), "T-approved-close");
    assert_eq!(task["status"], "approved");
    assert!(task.get("closed_at").is_none());
}

#[tokio::test]
async fn test_close_by_supervisor_does_not_echo_notification() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let head_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(&brehon_root, "T-approved-merge", "approved", "task");
    let mut seeded_task = read_test_task(&brehon_root, "T-approved-merge");
    seeded_task["assignee"] = "worker-1".into();
    seeded_task["review_owner"] = "worker-1".into();
    seeded_task["activity"] = "testing".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-approved-merge.json"),
        serde_json::to_string_pretty(&seeded_task).unwrap(),
    )
    .unwrap();
    write_review_metadata(&brehon_root, "T-approved-merge", "approved", &head_commit);
    // Supervisor close runs `verify_merge_ready`, which insists HEAD ==
    // merge_target. `init_git_workspace` leaves HEAD on `worker/test`; jump
    // back to `main` to mirror a real supervisor close from the main repo.
    run_git(workspace.path(), &["checkout", "main"]);

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-approved-merge"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let task = read_test_task(&brehon_root, "T-approved-merge");
    assert_eq!(task["status"], "merged");
    assert_eq!(task["closed_by"], "sup-1");
    assert_eq!(task["closed_role"], "supervisor");
    assert_eq!(task["merged_branch"], "main");
    assert_eq!(task["merged_commit"], head_commit);
    assert_eq!(task["assignee"], Value::Null);
    assert_eq!(task["review_owner"], Value::Null);
    assert!(task.get("activity").is_none());

    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["completion_mode"], "merge");
    assert_eq!(result_json["merged_branch"], "main");
    assert_eq!(result_json["merged_commit"], head_commit);
    assert!(result_json["message"]
        .as_str()
        .unwrap()
        .contains("Completion mode: merge"));

    let queued_for_supervisor: Vec<Value> = read_queued_prompts(&brehon_root)
        .into_iter()
        .filter(|payload| payload["target"] == "sup-1")
        .collect();
    assert!(
        queued_for_supervisor.is_empty(),
        "supervisor close should not enqueue a self-notification"
    );
}

#[tokio::test]
async fn test_close_by_supervisor_invalidates_stale_direct_merge_approval() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let reviewed_commit = init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("followup.txt"), "unreviewed\n").unwrap();
    run_git(workspace.path(), &["add", "followup.txt"]);
    run_git(workspace.path(), &["commit", "-m", "unreviewed followup"]);
    let latest_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(&brehon_root, "T-stale-direct-merge", "approved", "task");
    let mut task = read_test_task(&brehon_root, "T-stale-direct-merge");
    task["latest_commit"] = latest_commit.clone().into();
    task["merge_target"] = "main".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-stale-direct-merge.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata(
        &brehon_root,
        "T-stale-direct-merge",
        "approved",
        &reviewed_commit,
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-stale-direct-merge"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["error_code"], "stale_review_approval");
    assert_eq!(result_json["next_action"]["kind"], "request_review");
    let stored = read_test_task(&brehon_root, "T-stale-direct-merge");
    assert_eq!(stored["status"], "review_ready");
    assert_eq!(
        stored["stale_review"]["approved_review_commit"],
        reviewed_commit
    );
    assert_eq!(stored["stale_review"]["latest_commit"], latest_commit);
}

#[tokio::test]
async fn test_close_by_supervisor_rejects_merge_when_reviewed_commit_not_on_main() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "feature/test"]);
    std::fs::write(workspace.path().join("feature.txt"), "feature\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature"]);
    let feature_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(&brehon_root, "T-feature-review", "approved", "task");
    write_review_metadata(
        &brehon_root,
        "T-feature-review",
        "approved",
        &feature_commit,
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-feature-review"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("is not reachable from HEAD on main"));

    let task = read_test_task(&brehon_root, "T-feature-review");
    assert_eq!(task["status"], "approved");
    assert!(task.get("merged_commit").is_none());
}

#[tokio::test]
async fn test_close_by_supervisor_uses_last_reviewed_commit_when_request_tip_is_empty_marker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-empty-marker"],
    );
    std::fs::write(workspace.path().join("feature.txt"), "feature work\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);
    run_git(
        workspace.path(),
        &["merge", "--ff-only", "worker/task-empty-marker"],
    );
    run_git(workspace.path(), &["checkout", "worker/task-empty-marker"]);
    run_git(
        workspace.path(),
        &["commit", "--allow-empty", "-m", "empty resubmission marker"],
    );
    let empty_tip_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    assert_ne!(empty_tip_commit, reviewed_commit);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(
        &brehon_root,
        "T-approved-empty-marker-merge",
        "approved",
        "task",
    );
    write_review_metadata_with_commits(
        &brehon_root,
        "T-approved-empty-marker-merge",
        "approved",
        &empty_tip_commit,
        &[&reviewed_commit],
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-approved-empty-marker-merge"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let task = read_test_task(&brehon_root, "T-approved-empty-marker-merge");
    assert_eq!(task["status"], "merged");
    assert_eq!(task["merged_branch"], "main");
    assert_eq!(task["merged_commit"], reviewed_commit);
    assert_ne!(task["merged_commit"], empty_tip_commit);

    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["merged_commit"], reviewed_commit);
    assert_ne!(result_json["merged_commit"], empty_tip_commit);
}

#[tokio::test]
async fn test_close_by_supervisor_closes_approved_close_mode_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task_with_mode(
        root.path(),
        "T-approved-close-mode",
        "approved",
        "task",
        "close",
        "Audit-only task with no code changes",
    );

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-approved-close-mode"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["action"], "closed");
    assert_eq!(result_json["completion_mode"], "close");
    assert_eq!(result_json["worker_recycle_queued"], true);
    assert!(result_json["message"]
        .as_str()
        .unwrap()
        .contains("without a merge"));

    let task = read_test_task(root.path(), "T-approved-close-mode");
    assert_eq!(task["status"], "closed");
    assert_eq!(task["completion_mode"], "close");
    assert_eq!(task["closed_by"], "sup-1");
    assert_eq!(task["closed_role"], "supervisor");
    let recycle_requests = read_worker_recycle_requests(root.path());
    assert_eq!(recycle_requests.len(), 1);
    assert_eq!(recycle_requests[0]["task_id"], "T-approved-close-mode");
    assert_eq!(recycle_requests[0]["worker"], "worker-1");
}

#[tokio::test]
async fn test_close_rejects_corrupted_close_mode_phase_gate_with_merge_state() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task_with_mode(
        root.path(),
        "T-phase-gate-close-corrupt",
        "approved",
        "task",
        "close",
        "Imported phase gate for validation",
    );

    let mut task = read_test_task(root.path(), "T-phase-gate-close-corrupt");
    task["merge_target"] = "epic/test-feature".into();
    task["latest_commit"] = "abc123".into();
    task["plan_import"] = serde_json::json!({
        "is_phase_gate": true,
        "source_task_id": "12.G"
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-phase-gate-close-corrupt.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-phase-gate-close-corrupt"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("corrupted task state"),
        "{}",
        extract_text(&result)
    );

    let task = read_test_task(root.path(), "T-phase-gate-close-corrupt");
    assert_eq!(task["status"], "approved");
}

#[tokio::test]
async fn test_update_rejects_worker_merge_and_close_of_approved_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-approved-merge", "approved", "task");
    let merge_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-approved-merge",
            "status": "merged"
        }))
        .await
        .unwrap();
    assert_eq!(merge_result.is_error, Some(true));
    assert!(extract_text(&merge_result).contains("Only supervisors can set status to 'merged'"));

    write_test_task(root.path(), "T-approved-close", "approved", "task");
    let close_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-approved-close",
            "status": "closed"
        }))
        .await
        .unwrap();
    assert_eq!(close_result.is_error, Some(true));
    assert!(extract_text(&close_result).contains("Only supervisors can set status to 'closed'"));
}

#[tokio::test]
async fn test_update_rejects_non_assignee_worker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-2"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-owned-update", "in_progress", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-owned-update",
            "notes": "trying to update"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("assigned to 'worker-1' not 'worker-2'"),
        "{text}"
    );
    assert!(text.contains("Only the assigned worker can update"));

    let task = read_test_task(root.path(), "T-owned-update");
    assert_eq!(task["status"], "in_progress");
    assert!(task.get("notes").is_none());
}

#[tokio::test]
async fn test_update_allows_supervisor_to_set_close_mode_and_close_after_approval() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-update-close-mode", "approved", "task");

    let mode_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-update-close-mode",
            "completion_mode": "close"
        }))
        .await
        .unwrap();
    assert!(
        mode_result.is_error.is_none(),
        "{}",
        extract_text(&mode_result)
    );

    let close_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-update-close-mode",
            "status": "closed"
        }))
        .await
        .unwrap();
    assert!(
        close_result.is_error.is_none(),
        "{}",
        extract_text(&close_result)
    );

    let task = read_test_task(root.path(), "T-update-close-mode");
    assert_eq!(task["status"], "closed");
    assert_eq!(task["completion_mode"], "close");
}

#[tokio::test]
async fn test_update_rejects_supervisor_setting_merged_on_merge_flow_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-update-merge-mode", "approved", "task");

    let mut task = read_test_task(root.path(), "T-update-merge-mode");
    task["parent_id"] = "E-parent".into();
    task["merge_target"] = "epic/test-feature".into();
    task["latest_commit"] = "abc123".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-update-merge-mode.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-update-merge-mode",
            "status": "merged"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("cannot be set to 'merged' via task action=update"),
        "{}",
        extract_text(&result)
    );

    let stored = read_test_task(root.path(), "T-update-merge-mode");
    assert_eq!(stored["status"], "approved");
    assert_eq!(stored["integration_status"], "pending");
}

#[tokio::test]
async fn test_update_rejects_switching_merge_flow_task_to_close_mode() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-merge-flow-close", "approved", "task");

    let mut task = read_test_task(root.path(), "T-merge-flow-close");
    task["merge_target"] = "epic/test-feature".into();
    task["latest_commit"] = "abc123".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-merge-flow-close.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-merge-flow-close",
            "completion_mode": "close"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result)
            .contains("cannot be switched from completion_mode='merge' to 'close'"),
        "{}",
        extract_text(&result)
    );

    let task = read_test_task(root.path(), "T-merge-flow-close");
    assert_eq!(task["completion_mode"], "merge");
}

#[tokio::test]
async fn test_update_rejects_direct_approved_for_concrete_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-direct-approve", "in_review", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-direct-approve",
            "status": "approved"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("cannot be set to 'approved' via task action=update"),
        "{}",
        extract_text(&result)
    );

    let task = read_test_task(root.path(), "T-direct-approve");
    assert_eq!(task["status"], "in_review");
}

#[tokio::test]
async fn test_update_rejects_review_obligation_reset_to_pending() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-review-reset", "review_ready", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-review-reset",
            "status": "pending"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("cannot be reset to 'pending'"),
        "{}",
        extract_text(&result)
    );

    let task = read_test_task(root.path(), "T-review-reset");
    assert_eq!(task["status"], "review_ready");
}

#[tokio::test]
async fn test_update_rejects_direct_in_review_for_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-direct-review", "review_ready", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-direct-review",
            "status": "in_review"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("cannot be set to 'in_review' via task action=update"),
        "{}",
        extract_text(&result)
    );
    assert!(
        extract_text(&result).contains("verification action=request_review"),
        "{}",
        extract_text(&result)
    );

    let task = read_test_task(root.path(), "T-direct-review");
    assert_eq!(task["status"], "review_ready");
}

#[tokio::test]
async fn test_update_normalizes_pascal_case_status_for_storage() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-pascal-update", "Assigned", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-pascal-update",
            "status": "InProgress"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let task = read_test_task(root.path(), "T-pascal-update");
    assert_eq!(task["status"], "in_progress");
}

#[tokio::test]
async fn test_update_rejects_direct_assigned_transition() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-direct-assign", "pending", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-direct-assign",
            "status": "assigned"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        extract_text(&result).contains("Use factory action=assign_workers instead"),
        "{}",
        extract_text(&result)
    );

    let task = read_test_task(root.path(), "T-direct-assign");
    assert_eq!(task["status"], "pending");
}

#[tokio::test]
async fn test_update_ignores_task_type_changes() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-task-type", "pending", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-task-type",
            "title": "Updated title",
            "task_type": "epic"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let text = extract_text(&result);
    assert!(!text.contains("task_type"));

    let task = read_test_task(root.path(), "T-task-type");
    assert_eq!(task["task_type"], "task");
    assert_eq!(task["title"], "Updated title");
}

#[test]
fn test_epic_completion_requires_terminal_subtasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

    write_test_task(root.path(), "EPIC-1", "pending", "epic");
    write_test_task(root.path(), "T-merged", "merged", "task");
    write_test_task(root.path(), "T-approved", "approved", "task");

    let tasks_dir = root.path().join("runtime").join("tasks");
    let merged_path = tasks_dir.join("T-merged.json");
    let approved_path = tasks_dir.join("T-approved.json");

    let mut merged_task: Value =
        serde_json::from_str(&std::fs::read_to_string(&merged_path).unwrap()).unwrap();
    merged_task["parent_id"] = Value::String("EPIC-1".to_string());
    std::fs::write(
        &merged_path,
        serde_json::to_string_pretty(&merged_task).unwrap(),
    )
    .unwrap();

    let mut approved_task: Value =
        serde_json::from_str(&std::fs::read_to_string(&approved_path).unwrap()).unwrap();
    approved_task["parent_id"] = Value::String("EPIC-1".to_string());
    std::fs::write(
        &approved_path,
        serde_json::to_string_pretty(&approved_task).unwrap(),
    )
    .unwrap();

    let (total, closed, all_closed) = check_epic_completion("EPIC-1");
    assert_eq!(total, 2);
    assert_eq!(closed, 1);
    assert!(!all_closed);
}

#[test]
fn test_detect_default_branch_from_remote_head() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init", "-b", "feature"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);

    run_git(
        workspace.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/test.git",
        ],
    );
    run_git(
        workspace.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    let detected = detect_default_branch().unwrap();
    assert_eq!(detected, "main");
}

#[test]
fn test_git_probe_helpers_report_clean_worktree() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();

    init_git_workspace(workspace.path());

    assert!(!cherry_pick_in_progress_in(workspace.path()));
    assert_eq!(cherry_pick_sha_in(workspace.path()), None);
    assert!(unmerged_files(workspace.path()).unwrap().is_empty());
}

#[test]
fn test_git_probe_helpers_detect_in_progress_cherry_pick() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();

    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("conflict.txt"), "base\n").unwrap();
    run_git(workspace.path(), &["add", "conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "add conflict seed"]);

    run_git(workspace.path(), &["checkout", "-b", "feature/cherry-pick"]);
    std::fs::write(workspace.path().join("conflict.txt"), "feature branch\n").unwrap();
    run_git(workspace.path(), &["add", "conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature branch change"]);
    let cherry_pick_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    std::fs::write(workspace.path().join("conflict.txt"), "main branch\n").unwrap();
    run_git(workspace.path(), &["add", "conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "main branch change"]);

    let _stderr = run_git_expect_failure(workspace.path(), &["cherry-pick", &cherry_pick_commit]);

    assert!(cherry_pick_in_progress_in(workspace.path()));
    assert_eq!(
        cherry_pick_sha_in(workspace.path()),
        Some(cherry_pick_commit)
    );
    assert_eq!(
        unmerged_files(workspace.path()).unwrap(),
        vec!["conflict.txt".to_string()]
    );
}

#[test]
fn test_patch_equivalence_detects_same_delta_with_different_sha() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();

    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "feature/a"]);
    std::fs::write(workspace.path().join("delta.txt"), "shared delta\n").unwrap();
    run_git(workspace.path(), &["add", "delta.txt"]);
    run_git(workspace.path(), &["commit", "-m", "shared delta A"]);
    let commit_a = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["checkout", "-b", "feature/b"]);
    std::fs::write(workspace.path().join("delta.txt"), "shared delta\n").unwrap();
    run_git(workspace.path(), &["add", "delta.txt"]);
    run_git(workspace.path(), &["commit", "-m", "shared delta B"]);
    let commit_b = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    assert_ne!(commit_a, commit_b);
    assert_eq!(
        git_patch_id(workspace.path(), &commit_a).unwrap(),
        git_patch_id(workspace.path(), &commit_b).unwrap()
    );
    assert!(is_patch_equivalent_in_window_in(workspace.path(), &commit_a, "feature/b", 5).unwrap());
}

#[test]
fn test_patch_equivalence_rejects_non_equivalent_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();

    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "feature/a"]);
    std::fs::write(workspace.path().join("delta.txt"), "delta A\n").unwrap();
    run_git(workspace.path(), &["add", "delta.txt"]);
    run_git(workspace.path(), &["commit", "-m", "delta A"]);
    let commit_a = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["checkout", "-b", "feature/c"]);
    std::fs::write(workspace.path().join("delta.txt"), "delta C\n").unwrap();
    run_git(workspace.path(), &["add", "delta.txt"]);
    run_git(workspace.path(), &["commit", "-m", "delta C"]);

    assert!(
        !is_patch_equivalent_in_window_in(workspace.path(), &commit_a, "feature/c", 5).unwrap()
    );
}

#[test]
fn test_detect_default_branch_fallback_to_main() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init", "-b", "main"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);

    let detected = detect_default_branch().unwrap();
    assert_eq!(detected, "main");
}

#[test]
fn test_detect_default_branch_fallback_to_master() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init", "-b", "master"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);

    let detected = detect_default_branch().unwrap();
    assert_eq!(detected, "master");
}

#[test]
fn test_detect_default_branch_fallback_to_develop() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);

    run_git(workspace.path(), &["checkout", "-b", "develop"]);

    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);

    let detected = detect_default_branch().unwrap();
    assert_eq!(detected, "develop");
}

#[tokio::test]
async fn test_verify_merge_ready_uses_detected_branch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    Command::new("git")
        .args(["init"])
        .current_dir(workspace.path())
        .output()
        .unwrap();
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);

    Command::new("git")
        .args(["checkout", "-b", "develop"])
        .current_dir(workspace.path())
        .output()
        .unwrap();

    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);
    let head_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(&brehon_root, "T-develop-merge", "approved", "task");
    write_review_metadata(&brehon_root, "T-develop-merge", "approved", &head_commit);

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-develop-merge"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let task = read_test_task(&brehon_root, "T-develop-merge");
    assert_eq!(task["status"], "merged");
    assert_eq!(task["merged_branch"], "develop");

    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(result_json["merged_branch"], "develop");
}

#[tokio::test]
async fn test_subtask_merge_target_from_epic() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    // Create epic with integration_branch
    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/feature-xyz")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();

    let subtask_json = create_subtask_for_test(&tool, "Subtask 1", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    // Subtask should have merge_target set
    assert_eq!(subtask_json["merge_target"], "epic/feature-xyz");
    assert_eq!(subtask_json["integration_status"], "pending");

    // Verify saved task has the fields
    let saved_subtask = read_test_task(root.path(), subtask_id);
    assert_eq!(saved_subtask["merge_target"], "epic/feature-xyz");
    assert_eq!(saved_subtask["integration_status"], "pending");
}

#[test]
fn test_remote_status_unknown_when_no_remote() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init", "-b", "main"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);
    let head_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let status = detect_remote_merge_status(&head_commit, "main");
    assert_eq!(status, MergeStatus::RemoteStatusUnknown);
}

#[test]
fn test_remote_merge_status_unknown_when_remote_has_no_ref() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init", "-b", "main"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);
    let head_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(
        workspace.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/fake/repo.git",
        ],
    );

    let status = detect_remote_merge_status(&head_commit, "main");
    assert_eq!(status, MergeStatus::RemoteStatusUnknown);
}

#[test]
fn test_merged_locally_when_commit_not_on_remote() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init", "-b", "main"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);
    let head_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let clone = tempfile::tempdir().unwrap();
    run_git(clone.path(), &["init", "-b", "main"]);
    run_git(clone.path(), &["config", "user.email", "test@example.com"]);
    run_git(clone.path(), &["config", "user.name", "Test User"]);
    std::fs::write(clone.path().join("README.md"), "remote\n").unwrap();
    run_git(clone.path(), &["add", "README.md"]);
    run_git(clone.path(), &["commit", "-m", "remote seed"]);
    run_git(clone.path(), &["branch", "-M", "main"]);

    run_git(
        workspace.path(),
        &["remote", "add", "origin", clone.path().to_str().unwrap()],
    );
    run_git(workspace.path(), &["fetch", "origin"]);

    let status = detect_remote_merge_status(&head_commit, "main");
    assert_eq!(status, MergeStatus::MergedLocally);
}

#[test]
fn test_merged_remotely_when_commit_on_remote() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    let _env = ScopedEnv::set_with_defaults(&[(
        "BREHON_WORKSPACE_ROOT",
        workspace.path().to_str().unwrap(),
    )]);

    run_git(workspace.path(), &["init", "-b", "main"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("README.md"), "seed\n").unwrap();
    run_git(workspace.path(), &["add", "README.md"]);
    run_git(workspace.path(), &["commit", "-m", "seed"]);
    let head_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(remote.path(), &["init", "--bare"]);
    run_git(
        workspace.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    run_git(workspace.path(), &["push", "-u", "origin", "main"]);
    run_git(workspace.path(), &["fetch", "origin"]);

    let status = detect_remote_merge_status(&head_commit, "main");
    assert_eq!(status, MergeStatus::MergedRemotely);
}

#[test]
fn test_merge_status_display_strings_are_distinct() {
    assert_eq!(
        MergeStatus::MergedLocally.display(),
        "merged locally, not yet on remote"
    );
    assert_eq!(
        MergeStatus::MergedRemotely.display(),
        "merged locally and remotely"
    );
    assert_eq!(
        MergeStatus::RemoteStatusUnknown.display(),
        "merged locally, remote status unknown"
    );

    let displays: std::collections::HashSet<_> = [
        MergeStatus::MergedLocally.display(),
        MergeStatus::MergedRemotely.display(),
        MergeStatus::RemoteStatusUnknown.display(),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        displays.len(),
        3,
        "each variant must have distinct display string"
    );
}

#[tokio::test]
async fn test_close_includes_merge_status_in_result() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let head_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(&brehon_root, "T-merge-status", "approved", "task");
    write_review_metadata(&brehon_root, "T-merge-status", "approved", &head_commit);
    // verify_merge_ready requires HEAD == merge_target.
    run_git(workspace.path(), &["checkout", "main"]);

    let result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": "T-merge-status"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert!(result_json.get("merge_status").is_some());
    let merge_status = result_json["merge_status"].as_str().unwrap();
    assert!(
        merge_status == "merged locally, not yet on remote"
            || merge_status == "merged locally, remote status unknown",
        "unexpected merge_status: {merge_status}"
    );
    let message = result_json["message"].as_str().unwrap();
    assert!(
        message.contains("merged locally"),
        "message should contain merge scope: {message}"
    );
}

#[test]
fn test_close_result_message_uses_distinct_merge_status_strings() {
    use std::collections::HashSet;

    let locally_msg = format!("Task T-test {}.", MergeStatus::MergedLocally.display());
    let remotely_msg = format!("Task T-test {}.", MergeStatus::MergedRemotely.display());
    let unknown_msg = format!(
        "Task T-test {}.",
        MergeStatus::RemoteStatusUnknown.display()
    );

    assert!(
        locally_msg.contains("not yet on remote"),
        "MergedLocally message should be distinct: {locally_msg}"
    );
    assert!(
        remotely_msg.contains("and remotely"),
        "MergedRemotely message should be distinct: {remotely_msg}"
    );
    assert!(
        unknown_msg.contains("remote status unknown"),
        "RemoteStatusUnknown message should be distinct: {unknown_msg}"
    );

    let messages: HashSet<String> = HashSet::from([locally_msg, remotely_msg, unknown_msg]);
    assert_eq!(
        messages.len(),
        3,
        "all three status messages must be distinct"
    );
}

#[tokio::test]
async fn test_epic_with_integration_branch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let v = create_epic_for_test(&tool, "Feature Epic", Some("epic/T-abc123")).await;
    assert_eq!(v["task_type"], "epic");
    assert_eq!(v["integration_branch"], "epic/T-abc123");
    assert_eq!(v["completion_mode"], "close");
    let integration_worktree = v["integration_worktree"].as_str().unwrap();
    assert!(
        integration_worktree.contains("worktrees/epic"),
        "expected epic worktree path, got: {integration_worktree}"
    );
    assert!(Path::new(integration_worktree).exists());

    let task_id = v["task_id"].as_str().unwrap();
    let stored = read_test_task(root.path(), task_id);
    assert_eq!(stored["integration_branch"], "epic/T-abc123");
    assert_eq!(stored["integration_worktree"], integration_worktree);
    assert_eq!(
        run_git(
            Path::new(integration_worktree),
            &["branch", "--show-current"]
        ),
        "epic/T-abc123"
    );
}

#[tokio::test]
async fn test_implementation_epic_without_integration_branch_auto_gets_branch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let v = create_epic_for_test(&tool, "Regular Epic", None).await;
    let branch = v["integration_branch"].as_str().unwrap();
    assert!(branch.starts_with("epic/regular-epic-"));
    let integration_worktree = v["integration_worktree"].as_str().unwrap();
    assert!(Path::new(integration_worktree).exists());
    assert_eq!(
        run_git(
            Path::new(integration_worktree),
            &["branch", "--show-current"]
        ),
        branch
    );
}

#[tokio::test]
async fn test_audit_epic_without_integration_branch_stays_plain() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let v = create_plain_epic_for_test(&tool, "Audit Epic").await;
    assert!(v.get("integration_branch").is_none());
}

#[tokio::test]
async fn test_implementation_epic_can_explicitly_opt_into_direct_to_main() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "title": "Maintenance Epic",
            "direct_to_main": true,
            "description": "Maintenance Epic coordinates related implementation work.",
            "acceptance_criteria": [
                "Maintenance Epic tracks implementation subtasks",
                "Maintenance Epic remains direct-to-main by explicit policy"
            ],
            "plan_steps": [
                "Create the maintenance epic",
                "Track and validate subtask progress"
            ]
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let epic: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert!(epic.get("integration_branch").is_none());
    assert_eq!(epic["direct_to_main"], true);
}

#[tokio::test]
async fn test_subtask_inherits_merge_target_from_feature_epic() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let epic = create_epic_for_test(&tool, "Feature Epic", Some("epic/feature-auth")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Implement login", epic_id).await;
    assert_eq!(subtask["merge_target"], "epic/feature-auth");
    assert_eq!(subtask["integration_status"], "pending");

    let subtask_id = subtask["task_id"].as_str().unwrap();
    let stored = read_test_task(root.path(), subtask_id);
    assert_eq!(stored["merge_target"], "epic/feature-auth");
    assert_eq!(stored["integration_status"], "pending");
}

#[tokio::test]
async fn test_merge_subtask_under_plain_epic_requires_direct_to_main_opt_in() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let epic = create_plain_epic_for_test(&tool, "Audit Epic").await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": "Subtask",
            "parent_id": epic_id,
            "description": "Subtask implements one concrete outcome.",
            "acceptance_criteria": ["Subtask completes its assigned outcome"],
            "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
            "test_requirements": ["cargo test -p brehon-mcp"],
            "plan_steps": ["Inspect current state", "Implement the change", "Verify the result"]
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("direct_to_main=true"),
        "expected direct_to_main guidance, got: {text}"
    );
}

#[tokio::test]
async fn test_merge_subtask_under_plain_epic_allows_explicit_direct_to_main() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let epic = create_plain_epic_for_test(&tool, "Audit Epic").await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_direct_to_main_subtask_for_test(&tool, "Subtask", epic_id).await;
    assert_eq!(subtask["merge_target"], "main");
    assert_eq!(subtask["direct_to_main"], true);
    assert!(subtask.get("integration_status").is_none());
}

#[tokio::test]
async fn test_update_integration_branch_on_epic() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(root.path());
    std::fs::write(root.path().join(".gitignore"), ".brehon/\n").unwrap();
    run_git(root.path(), &["add", ".gitignore"]);
    run_git(root.path(), &["commit", "-m", "ignore brehon runtime"]);
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(&brehon_root, "EPIC-test", "pending", "epic");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "EPIC-test",
            "integration_branch": "epic/new-branch"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let task = read_test_task(&brehon_root, "EPIC-test");
    assert_eq!(task["integration_branch"], "epic/new-branch");
    let integration_worktree = task["integration_worktree"].as_str().unwrap();
    assert!(Path::new(integration_worktree).exists());
    assert_eq!(
        run_git(
            Path::new(integration_worktree),
            &["branch", "--show-current"]
        ),
        "epic/new-branch"
    );
}

#[tokio::test]
async fn test_update_integration_status_on_subtask() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    let epic = create_epic_for_test(&tool, "Feature Epic", Some("epic/feature-x")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Subtask", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    let update_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": subtask_id,
            "integration_status": "integrated"
        }))
        .await
        .unwrap();

    assert!(update_result.is_error.is_none());
    let stored = read_test_task(root.path(), subtask_id);
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["merge_target"], "epic/feature-x");
}

#[tokio::test]
async fn test_epic_with_empty_integration_branch_auto_generates_for_implementation_epic() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let v = create_epic_for_test(&tool, "Epic with empty branch", Some("")).await;
    let branch = v["integration_branch"].as_str().unwrap();
    assert!(branch.starts_with("epic/epic-with-empty-branch-"));
}

#[tokio::test]
async fn test_subtask_under_epic_with_cleared_integration_branch_requires_direct_to_main() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    // Create epic with integration_branch
    let epic = create_epic_for_test(&tool, "Epic", Some("epic/test")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Subtask", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    // Now clear the integration_branch
    let clear_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": epic_id,
            "integration_branch": ""
        }))
        .await
        .unwrap();

    assert!(
        clear_result.is_error.is_none(),
        "clearing integration_branch should succeed: {}",
        extract_text(&clear_result)
    );
    let cleared_epic = read_test_task(root.path(), epic_id);
    assert!(cleared_epic.get("integration_branch").is_none());
    assert!(cleared_epic.get("integration_worktree").is_none());

    // Create another subtask - should now be rejected unless explicitly direct_to_main
    let blocked_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": "Subtask 2",
            "parent_id": epic_id,
            "description": "Subtask 2 implements one concrete outcome.",
            "acceptance_criteria": ["Subtask 2 completes its assigned outcome"],
            "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
            "test_requirements": ["cargo test -p brehon-mcp"],
            "plan_steps": ["Inspect current state", "Implement the change", "Verify the result"]
        }))
        .await
        .unwrap();
    assert_eq!(blocked_result.is_error, Some(true));
    let text = extract_text(&blocked_result);
    assert!(
        text.contains("direct_to_main=true"),
        "expected direct_to_main guidance, got: {text}"
    );

    let subtask2 = create_direct_to_main_subtask_for_test(&tool, "Subtask 2", epic_id).await;
    assert_eq!(subtask2["merge_target"], "main");
    assert_eq!(subtask2["direct_to_main"], true);
    assert!(subtask2.get("integration_status").is_none());

    // Original subtask still has its values
    let stored = read_test_task(root.path(), subtask_id);
    assert_eq!(stored["merge_target"], "epic/test");
}

#[tokio::test]
async fn test_integration_status_invalid_value_rejected() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    let epic = create_epic_for_test(&tool, "Epic", Some("epic/test")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Subtask", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    // Try to set invalid integration_status
    let invalid_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": subtask_id,
            "integration_status": "invalid_status"
        }))
        .await
        .unwrap();

    assert!(invalid_result.is_error == Some(true));
    let text = extract_text(&invalid_result);
    assert!(
        text.contains("Invalid integration_status"),
        "expected error about invalid status, got: {text}"
    );
    assert!(
        text.contains("pending, integrated, not_applicable"),
        "expected valid values in error, got: {text}"
    );

    // Verify the status was not changed
    let stored = read_test_task(root.path(), subtask_id);
    assert_eq!(stored["integration_status"], "pending");
}

#[tokio::test]
async fn test_integration_branch_only_settable_on_containers() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-regular", "pending", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-regular",
            "integration_branch": "epic/should-fail"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("integration_branch can only be set on initiatives or epics"),
        "expected error about containers-only, got: {text}"
    );
}

#[tokio::test]
async fn test_update_integration_branch_on_initiative() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(root.path());
    std::fs::write(root.path().join(".gitignore"), ".brehon/\n").unwrap();
    run_git(root.path(), &["add", ".gitignore"]);
    run_git(root.path(), &["commit", "-m", "ignore brehon runtime"]);
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(&brehon_root, "I-test", "pending", "initiative");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "I-test",
            "integration_branch": "initiative/test-program"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let stored = read_test_task(&brehon_root, "I-test");
    assert_eq!(stored["integration_branch"], "initiative/test-program");
    let integration_worktree = stored["integration_worktree"].as_str().unwrap();
    assert!(integration_worktree.contains("worktrees/initiative"));
    assert!(Path::new(integration_worktree).exists());
    assert_eq!(
        run_git(
            Path::new(integration_worktree),
            &["branch", "--show-current"]
        ),
        "initiative/test-program"
    );
}

#[tokio::test]
async fn test_merge_target_only_settable_on_subtasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-standalone", "pending", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-standalone",
            "merge_target": "epic/test"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("merge_target can only be set on subtasks with valid parent_id"),
        "expected error about valid parent_id, got: {text}"
    );
}

#[tokio::test]
async fn test_integration_status_only_settable_on_valid_subtasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();
    write_test_task(root.path(), "T-standalone", "pending", "task");

    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-standalone",
            "integration_status": "integrated"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("integration_status can only be set on subtasks with valid parent_id"),
        "expected error about valid parent_id, got: {text}"
    );
}

#[tokio::test]
async fn test_integration_fields_rejected_on_task_with_empty_parent_id() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    // Create a task with empty parent_id (malformed data)
    let tasks_dir = root.path().join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let malformed_task = serde_json::json!({
        "task_id": "T-malformed",
        "title": "Malformed task",
        "status": "pending",
        "task_type": "task",
        "parent_id": "",
        "completion_mode": "merge"
    });
    std::fs::write(
        tasks_dir.join("T-malformed.json"),
        serde_json::to_string_pretty(&malformed_task).unwrap(),
    )
    .unwrap();

    // Try to set merge_target - should fail because parent_id is empty
    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-malformed",
            "merge_target": "epic/test"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("merge_target can only be set on subtasks with valid parent_id"),
        "expected error for empty parent_id, got: {text}"
    );

    // Try to set integration_status - should also fail
    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-malformed",
            "integration_status": "pending"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("integration_status can only be set on subtasks with valid parent_id"),
        "expected error for empty parent_id, got: {text}"
    );
}

#[tokio::test]
async fn test_integration_fields_rejected_on_task_with_nonexistent_parent() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    // Create a task referencing a non-existent parent
    let tasks_dir = root.path().join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let orphan_task = serde_json::json!({
        "task_id": "T-orphan",
        "title": "Orphan task",
        "status": "pending",
        "task_type": "task",
        "parent_id": "EPIC-nonexistent",
        "completion_mode": "merge"
    });
    std::fs::write(
        tasks_dir.join("T-orphan.json"),
        serde_json::to_string_pretty(&orphan_task).unwrap(),
    )
    .unwrap();

    // Try to set merge_target - should fail because parent doesn't exist
    let result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": "T-orphan",
            "merge_target": "epic/test"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("merge_target can only be set on subtasks with valid parent_id"),
        "expected error for nonexistent parent, got: {text}"
    );
}

#[tokio::test]
async fn test_direct_update_merge_target_on_subtask() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    // Create epic
    let epic = create_epic_for_test(&tool, "Epic", Some("epic/original")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Subtask", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    // Verify defaults
    let stored = read_test_task(root.path(), subtask_id);
    assert_eq!(stored["merge_target"], "epic/original");

    // Directly update merge_target
    let update_result = tool
        .execute(serde_json::json!({
            "action": "update",
            "id": subtask_id,
            "merge_target": "epic/changed"
        }))
        .await
        .unwrap();

    assert!(
        update_result.is_error.is_none(),
        "{}",
        extract_text(&update_result)
    );
    let updated = read_test_task(root.path(), subtask_id);
    assert_eq!(updated["merge_target"], "epic/changed");
}

#[tokio::test]
async fn test_standalone_task_has_no_merge_target_nor_integration_status() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    let v = create_standalone_task_for_test(&tool, "Standalone task (no parent)").await;
    // Standalone tasks don't have merge_target or integration_status
    assert!(
        v.get("merge_target").is_none(),
        "standalone task should not have merge_target"
    );
    assert!(
        v.get("integration_status").is_none(),
        "standalone task should not have integration_status"
    );

    let task_id = v["task_id"].as_str().unwrap();
    let stored = read_test_task(root.path(), task_id);
    assert!(stored.get("merge_target").is_none());
    assert!(stored.get("integration_status").is_none());
}

#[tokio::test]
async fn test_epic_close_rejects_when_subtask_not_integrated() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    // Create a plain audit epic so there is no integration_branch check.
    let epic = create_plain_epic_for_test(&tool, "Regular Epic").await;
    let epic_id = epic["task_id"].as_str().unwrap();

    // Create a direct-to-main subtask but don't close it (so epic has open subtask)
    let _ = create_direct_to_main_subtask_for_test(&tool, "Subtask 1", epic_id).await;

    // Try to close epic - should fail because subtask not terminal
    let close_result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();

    assert_eq!(close_result.is_error, Some(true));
    let text = extract_text(&close_result);
    assert!(
        text.contains("subtasks closed") || text.contains("Close all subtasks"),
        "expected error about subtasks, got: {text}"
    );
}

#[tokio::test]
async fn test_feature_epic_close_rejects_when_subtask_not_integrated() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    // Create epic with integration branch
    let epic = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Subtask 1", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    // Close the subtask first (so terminal status), but integration_status still pending
    let mut task = read_test_task(root.path(), subtask_id);
    task["status"] = "closed".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    // Try to close epic - should fail because subtask not integrated
    // (integration check happens before branch verification)
    let close_result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();

    assert_eq!(close_result.is_error, Some(true));
    let text = extract_text(&close_result);
    assert!(
        text.contains("children integrated") || text.contains("not yet integrated"),
        "expected error about integration status, got: {text}"
    );
}

#[tokio::test]
async fn test_guard_rail_blocks_direct_to_main_for_epic_subtask() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    // Create feature epic
    let epic = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Subtask", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();
    assert_eq!(subtask["merge_target"], "epic/test-feature");

    // Make a commit
    std::fs::write(workspace.path().join("test.txt"), "test\n").unwrap();
    run_git(workspace.path(), &["add", "test.txt"]);
    run_git(workspace.path(), &["commit", "-m", "test change"]);
    let commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    // Update subtask to approved and set commit
    let mut task = read_test_task(root.path(), subtask_id);
    task["status"] = "approved".into();
    task["latest_commit"] = commit.clone().into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    // Write review metadata
    write_review_metadata(root.path(), subtask_id, "approved", &commit);

    // Try to close - should fail because merge_target is epic branch, not main
    let close_result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(close_result.is_error, Some(true));
    let text = extract_text(&close_result);
    assert!(
        text.contains("merge_target") && text.contains("epic branch"),
        "expected error about epic branch merge_target, got: {text}"
    );
}

#[tokio::test]
async fn test_integrate_action_cherry_picks_reviewed_commit_into_epic_branch_and_closes_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(workspace.path(), &["checkout", "-b", "worker/task-1"]);
    std::fs::write(workspace.path().join("feature.txt"), "feature work\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_result = tool
            .execute(serde_json::json!({
                "action": "create",
                "task_type": "epic",
                "title": "Feature Epic",
                "description": "Integrate reviewed subtasks through an epic branch.",
                "acceptance_criteria": ["Subtasks integrate into epic/test-feature", "Epic closes only after integration"],
                "plan_steps": ["Create epic branch flow", "Integrate approved subtasks"],
                "integration_branch": "epic/test-feature"
            }))
            .await
            .unwrap();
    assert!(
        epic_result.is_error.is_none(),
        "{}",
        extract_text(&epic_result)
    );
    let epic: Value = serde_json::from_str(&extract_text(&epic_result)).unwrap();
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask_result = tool
            .execute(serde_json::json!({
                "action": "create",
                "title": "Epic Subtask",
                "parent_id": epic_id,
                "description": "Cherry-pick the approved commit onto the epic branch and record it.",
                "acceptance_criteria": ["Reviewed commit lands on epic/test-feature", "Task records integrated terminal state"],
                "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
                "test_requirements": ["cargo test -p brehon-mcp --lib tools::task_actions::tests::test_integrate_action_cherry_picks_reviewed_commit_into_epic_branch_and_closes_task -- --exact"],
                "plan_steps": ["Verify approved state", "Integrate reviewed commit", "Persist closed+integrated state"]
            }))
            .await
            .unwrap();
    assert!(
        subtask_result.is_error.is_none(),
        "{}",
        extract_text(&subtask_result)
    );
    let subtask: Value = serde_json::from_str(&extract_text(&subtask_result)).unwrap();
    let subtask_id = subtask["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    task["assignee"] = "worker-1".into();
    task["review_owner"] = "worker-1".into();
    task["activity"] = "testing".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata(&brehon_root, subtask_id, "approved", &reviewed_commit);

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrated");
    assert_eq!(result["terminal_status"], "closed");
    assert_eq!(result["schema_version"], 1);
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(result["status"], "integrated");
    assert_eq!(result["merge_target"], "epic/test-feature");
    assert_eq!(result["merged_branch"], "epic/test-feature");
    assert_eq!(result["reviewed_commit"], reviewed_commit);
    assert_eq!(result["integration_status"], "integrated");
    assert_eq!(result["conflicting_files"], serde_json::json!([]));
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );
    assert_eq!(result["cherry_pick_head"], Value::Null);
    assert_eq!(result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(result["worker_recycle_queued"], true);
    assert_eq!(result["closed_by"], "sup-1");
    assert_eq!(result["closed_role"], "supervisor");
    assert_eq!(result["parent_epic"]["epic_id"], epic_id);
    let worktree_path = result["worktree_path"].as_str().unwrap();
    let integration_worktree = result["integration_worktree"].as_str().unwrap();
    assert_eq!(worktree_path, integration_worktree);
    assert!(Path::new(integration_worktree).exists());

    let epic_head = run_git(Path::new(integration_worktree), &["rev-parse", "HEAD"]);
    assert_eq!(result["merged_commit"], epic_head);
    assert_eq!(
        run_git(workspace.path(), &["branch", "--show-current"]),
        "main",
        "supervisor workspace should stay on its own branch instead of checking out the epic branch in place"
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["merged_branch"], "epic/test-feature");
    assert_eq!(stored["merged_commit"], epic_head);
    assert_eq!(stored["assignee"], Value::Null);
    assert_eq!(stored["review_owner"], Value::Null);
    assert!(stored.get("activity").is_none());
    let recycle_requests = read_worker_recycle_requests(&brehon_root);
    assert_eq!(recycle_requests.len(), 1);
    assert_eq!(recycle_requests[0]["task_id"], subtask_id);
    assert_eq!(recycle_requests[0]["worker"], "worker-1");
    assert!(
        !brehon_root
            .join("runtime")
            .join("reviews")
            .join(subtask_id)
            .join("state.json")
            .exists(),
        "integrated subtasks should clear their persisted review state"
    );

    let feature_on_epic = run_git(
        Path::new(integration_worktree),
        &["show", "HEAD:feature.txt"],
    );
    assert_eq!(feature_on_epic, "feature work");
}

#[tokio::test]
async fn test_integrate_action_invalidates_stale_approval_when_latest_commit_changed() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-stale"]);
    run_git(workspace.path(), &["checkout", "-b", "worker/stale-task"]);
    std::fs::write(workspace.path().join("feature.txt"), "reviewed work\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "reviewed work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    std::fs::write(
        workspace.path().join("feature.txt"),
        "unreviewed followup\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "unreviewed followup"]);
    let latest_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic: Value = serde_json::from_str(&extract_text(
        &tool
            .execute(serde_json::json!({
                "action": "create",
                "task_type": "epic",
                "title": "Stale Approval Epic",
                "description": "Integration must reject stale approvals.",
                "acceptance_criteria": ["Stale approvals cannot integrate"],
                "plan_steps": ["Create reviewed commit", "Create unreviewed checkpoint"],
                "integration_branch": "epic/test-stale"
            }))
            .await
            .unwrap(),
    ))
    .unwrap();
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask: Value = serde_json::from_str(&extract_text(
        &tool
            .execute(serde_json::json!({
                "action": "create",
                "title": "Stale Approval Subtask",
                "parent_id": epic_id,
                "description": "Approved review is no longer current.",
                "acceptance_criteria": ["Integration refuses stale review"],
                "file_hints": ["feature.txt"],
                "test_requirements": ["cargo test -p brehon-mcp --lib tools::task_actions::tests::test_integrate_action_invalidates_stale_approval_when_latest_commit_changed -- --exact"],
                "plan_steps": ["Attempt integration", "Require rereview"]
            }))
            .await
            .unwrap(),
    ))
    .unwrap();
    let subtask_id = subtask["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    task["latest_commit"] = latest_commit.clone().into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata(&brehon_root, subtask_id, "approved", &reviewed_commit);

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(integrate_result.is_error, Some(true));
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["error_code"], "stale_review_approval");
    assert_eq!(
        result["next_action_for_supervisor"]["kind"],
        "request_review"
    );
    assert_eq!(
        result["next_action_for_supervisor"]["args"]["task_id"],
        subtask_id
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "review_ready");
    assert_eq!(
        stored["stale_review"]["approved_review_commit"],
        reviewed_commit
    );
    assert_eq!(stored["stale_review"]["latest_commit"], latest_commit);
}

#[tokio::test]
async fn test_integrate_action_complete_phase_is_idempotent_without_git_side_effects() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(workspace.path(), &["checkout", "-b", "worker/task-1"]);
    std::fs::write(workspace.path().join("feature.txt"), "feature work\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Epic Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    task["assignee"] = "worker-1".into();
    task["review_owner"] = "worker-1".into();
    task["activity"] = "testing".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata(&brehon_root, subtask_id, "approved", &reviewed_commit);

    let first_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        first_integrate.is_error.is_none(),
        "{}",
        extract_text(&first_integrate)
    );
    let first_result: Value = serde_json::from_str(&extract_text(&first_integrate)).unwrap();
    assert_eq!(first_result["integration_phase"], "complete");
    assert_eq!(first_result["status"], "integrated");
    let integration_worktree = first_result["integration_worktree"].as_str().unwrap();
    let worktree_mtime_before = std::fs::metadata(integration_worktree)
        .unwrap()
        .modified()
        .unwrap();

    let second_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        second_integrate.is_error.is_none(),
        "{}",
        extract_text(&second_integrate)
    );
    let second_result: Value = serde_json::from_str(&extract_text(&second_integrate)).unwrap();
    assert_eq!(second_result["action"], "integrated");
    assert_eq!(second_result["terminal_status"], "closed");
    assert_eq!(second_result["integration_phase"], "complete");
    assert_eq!(second_result["status"], "already_integrated");
    assert_eq!(second_result["merge_target"], "epic/test-feature");
    assert_eq!(second_result["merged_branch"], "epic/test-feature");
    assert_eq!(
        second_result["merged_commit"],
        first_result["merged_commit"]
    );
    assert_eq!(second_result["integration_status"], "integrated");
    assert_eq!(
        second_result["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );
    assert_eq!(second_result["conflicting_files"], serde_json::json!([]));
    assert_eq!(second_result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(second_result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(second_result["already_integrated"], Value::Bool(true));
    assert_eq!(
        std::fs::metadata(integration_worktree)
            .unwrap()
            .modified()
            .unwrap(),
        worktree_mtime_before,
        "idempotent complete-phase retry should not touch the integration worktree",
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
    assert_eq!(stored["merged_commit"], first_result["merged_commit"]);
    assert_eq!(read_worker_recycle_requests(&brehon_root).len(), 1);
}

#[tokio::test]
async fn test_integrate_action_cherry_picks_full_reviewed_commit_set() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    let epic_base = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "-b", "worker/task-2"]);

    std::fs::write(workspace.path().join("feature.txt"), "part 1\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature part 1"]);
    let first_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    std::fs::write(workspace.path().join("feature.txt"), "part 1\npart 2\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature part 2"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Epic Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&first_commit, &reviewed_commit],
    );
    let request = read_current_review_request(subtask_id).expect("review metadata should exist");
    assert_eq!(
        reviewed_commits(&request),
        vec![first_commit.clone(), reviewed_commit.clone()]
    );

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrated");
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(result["status"], "integrated");
    assert_eq!(result["conflicting_files"], serde_json::json!([]));
    assert_eq!(result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([first_commit, reviewed_commit])
    );
    let integration_worktree = result["integration_worktree"].as_str().unwrap();

    let feature_on_epic = run_git(
        Path::new(integration_worktree),
        &["show", "HEAD:feature.txt"],
    );
    assert_eq!(feature_on_epic, "part 1\npart 2");
    let ahead_count = run_git(
        Path::new(integration_worktree),
        &["rev-list", "--count", &format!("{epic_base}..HEAD")],
    );
    assert_eq!(ahead_count, "2");
    let current_base = run_git(
        Path::new(integration_worktree),
        &["merge-base", &epic_base, "HEAD"],
    );
    assert_eq!(current_base, epic_base);
}

#[tokio::test]
async fn test_integrate_action_treats_resolved_empty_reviewed_set_as_noop() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    let epic_base = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-empty-tip"],
    );
    run_git(
        workspace.path(),
        &["commit", "--allow-empty", "-m", "empty reviewed tip"],
    );
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Empty Tip Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits_and_empty_flag(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[],
        true,
    );

    let request = read_current_review_request(subtask_id).expect("review metadata should exist");
    assert!(request.resolved_empty_commit_set);
    assert!(reviewed_commits(&request).is_empty());

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrated");
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(result["status"], "integrated");
    let integration_worktree = result["integration_worktree"].as_str().unwrap();
    assert_eq!(result["already_integrated"], Value::Bool(true));
    assert_eq!(result["conflicting_files"], serde_json::json!([]));
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );
    assert_eq!(result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");

    let head_after = run_git(Path::new(integration_worktree), &["rev-parse", "HEAD"]);
    assert_eq!(head_after, epic_base);
    let ahead_count = run_git(
        Path::new(integration_worktree),
        &["rev-list", "--count", &format!("{epic_base}..HEAD")],
    );
    assert_eq!(ahead_count, "0");
}

#[tokio::test]
async fn test_integrate_action_rejects_resolved_empty_reviewed_set_when_task_not_approved() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-empty-tip-rejected"],
    );
    run_git(
        workspace.path(),
        &["commit", "--allow-empty", "-m", "empty reviewed tip"],
    );
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Empty Tip Not Approved", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "in_review".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits_and_empty_flag(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[],
        true,
    );

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(integrate_result.is_error, Some(true));
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrate");
    assert_eq!(result["status"], "error");
    assert_eq!(result["error_code"], "integration_requires_approved_status");
    assert_eq!(result["current_status"], "in_review");
    assert_eq!(result["integration_phase"], "null");
    assert_eq!(
        result["next_action_for_supervisor"]["kind"],
        "approve_first"
    );
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "in_review");
    assert_eq!(stored["integration_status"], "pending");
    assert!(
        stored.get("closed_at").is_none(),
        "empty reviewed-set shortcut must not close non-approved tasks"
    );
}

#[tokio::test]
async fn test_integrate_action_skips_already_applied_prior_reviewed_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-rereview"],
    );
    std::fs::write(workspace.path().join("src.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "prior reviewed implementation"],
    );
    let prior_reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    std::fs::write(workspace.path().join("src.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "merge target already has implementation"],
    );

    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    let epic_base = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "worker/task-rereview"]);
    std::fs::write(workspace.path().join("notes.md"), "follow-up delta\n").unwrap();
    run_git(workspace.path(), &["add", "notes.md"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "follow-up review delta"],
    );
    let followup_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Re-review Delta Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &followup_commit,
        &[&prior_reviewed_commit, &followup_commit],
    );

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrated");
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(result["status"], "integrated");
    assert_eq!(result["conflicting_files"], serde_json::json!([]));
    assert_eq!(result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([prior_reviewed_commit, followup_commit])
    );
    let integration_worktree = result["integration_worktree"].as_str().unwrap();
    assert_eq!(
        result["reviewed_commit"],
        Value::String(followup_commit.clone())
    );
    assert_eq!(
        run_git(Path::new(integration_worktree), &["show", "HEAD:src.txt"]),
        "shared implementation"
    );
    assert_eq!(
        run_git(Path::new(integration_worktree), &["show", "HEAD:notes.md"]),
        "follow-up delta"
    );
    let ahead_count = run_git(
        Path::new(integration_worktree),
        &["rev-list", "--count", &format!("{epic_base}..HEAD")],
    );
    assert_eq!(ahead_count, "1");
}

#[tokio::test]
async fn test_integrate_action_long_lived_epic_still_picks_followup_after_patch_equivalent_prior_commit(
) {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-rereview-window"],
    );
    std::fs::write(workspace.path().join("src.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "prior reviewed implementation"],
    );
    let prior_reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    std::fs::write(workspace.path().join("src.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "merge target already has implementation"],
    );

    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    for index in 0..49 {
        run_git(
            workspace.path(),
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("epic filler {index}"),
            ],
        );
    }
    let epic_base = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(
        workspace.path(),
        &["checkout", "worker/task-rereview-window"],
    );
    std::fs::write(workspace.path().join("notes.md"), "follow-up delta\n").unwrap();
    run_git(workspace.path(), &["add", "notes.md"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "follow-up review delta"],
    );
    let followup_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json =
        create_subtask_for_test(&tool, "Long-lived Re-review Delta Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &followup_commit,
        &[&prior_reviewed_commit, &followup_commit],
    );

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrated");
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(result["status"], "integrated");
    assert_eq!(result["conflicting_files"], serde_json::json!([]));
    assert_eq!(result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([prior_reviewed_commit, followup_commit])
    );
    let integration_worktree = result["integration_worktree"].as_str().unwrap();

    assert_eq!(
        run_git(Path::new(integration_worktree), &["show", "HEAD:src.txt"]),
        "shared implementation"
    );
    assert_eq!(
        run_git(Path::new(integration_worktree), &["show", "HEAD:notes.md"]),
        "follow-up delta"
    );
    let ahead_count = run_git(
        Path::new(integration_worktree),
        &["rev-list", "--count", &format!("{epic_base}..HEAD")],
    );
    assert_eq!(ahead_count, "1");
}

#[tokio::test]
async fn test_integrate_action_escalates_conflict_to_supervisor_owned_state() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    run_git(workspace.path(), &["checkout", "main"]);

    run_git(workspace.path(), &["checkout", "-b", "worker/task-3"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker branch change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Conflicting Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&reviewed_commit],
    );

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(integrate_result.is_error, Some(true));
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["schema_version"], 1);
    assert_eq!(result["integration_phase"], "cherry_picking");
    assert_eq!(result["status"], "waiting_for_supervisor");
    assert_eq!(
        result["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );
    assert_eq!(result["cherry_pick_head"], reviewed_commit);
    assert_eq!(
        result["next_action_for_supervisor"]["kind"],
        "resolve_and_continue"
    );
    assert_eq!(result["next_action_for_brehon"]["kind"], "detect_on_retry");

    let stored = read_test_task(&brehon_root, subtask_id);
    // Task status remains approved; integration state machine tracks the conflict.
    assert_eq!(stored["status"], "approved");
    assert_eq!(stored["integration"]["phase"], "cherry_picking");
    assert_eq!(
        stored["integration"]["conflicting_files"][0],
        "src/conflict.txt"
    );
    assert!(
        !brehon_root
            .join("runtime")
            .join("reviews")
            .join(subtask_id)
            .join("state.json")
            .exists(),
        "integration conflict escalation should clear persisted review state"
    );
}

#[tokio::test]
async fn test_integration_continue_via_cherry_pick_continue_succeeds() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    run_git(workspace.path(), &["checkout", "main"]);

    run_git(workspace.path(), &["checkout", "-b", "worker/task-resume"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker branch change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json =
        create_subtask_for_test(&tool, "Retrying Conflicting Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&reviewed_commit],
    );

    let first_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(first_integrate.is_error, Some(true));
    let first_result: Value = serde_json::from_str(&extract_text(&first_integrate)).unwrap();
    assert_eq!(first_result["integration_phase"], "cherry_picking");
    assert_eq!(first_result["status"], "waiting_for_supervisor");
    assert_eq!(
        first_result["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    assert_eq!(
        first_result["next_action_for_supervisor"]["kind"],
        "resolve_and_continue"
    );
    assert_eq!(
        first_result["next_action_for_brehon"]["kind"],
        "detect_on_retry"
    );
    let integration_worktree = first_result["worktree_path"].as_str().unwrap();

    assert!(
        read_current_review_request(subtask_id).is_some(),
        "retry should keep the approved review request metadata"
    );

    std::fs::write(
        Path::new(integration_worktree).join("src/conflict.txt"),
        "epic branch\nworker branch\n",
    )
    .unwrap();
    run_git(
        Path::new(integration_worktree),
        &["add", "src/conflict.txt"],
    );
    run_git(
        Path::new(integration_worktree),
        &["cherry-pick", "--continue"],
    );

    let second_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        second_integrate.is_error.is_none(),
        "{}",
        extract_text(&second_integrate)
    );
    let second_result: Value = serde_json::from_str(&extract_text(&second_integrate)).unwrap();
    assert_eq!(second_result["action"], "integrated");
    assert_eq!(second_result["integration_phase"], "complete");
    assert_eq!(second_result["status"], "integrated");
    assert_eq!(second_result["terminal_status"], "closed");
    assert_eq!(second_result["conflicting_files"], serde_json::json!([]));
    assert_eq!(second_result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(second_result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(second_result["reviewed_commit"], reviewed_commit);
    assert_eq!(
        second_result["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );
    let merged_commit = second_result["merged_commit"].as_str().unwrap();

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
    assert_eq!(stored["merged_commit"], merged_commit);
    assert_eq!(
        run_git(
            Path::new(integration_worktree),
            &["show", "HEAD:src/conflict.txt"],
        ),
        "epic branch\nworker branch"
    );
    assert_eq!(
        run_git(Path::new(integration_worktree), &["rev-parse", "HEAD"]),
        merged_commit
    );
    assert!(
        run_git(
            Path::new(integration_worktree),
            &["log", "-1", "--format=%B"]
        )
        .contains(&format!("(cherry picked from commit {reviewed_commit})")),
        "expected integrated epic-branch commit to retain the reviewed-commit trailer"
    );
}

#[tokio::test]
async fn test_integrate_action_accepts_mixed_patch_equivalent_and_trailer_proofs_after_retry() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-mixed-proof-retry"],
    );
    std::fs::write(
        workspace.path().join("shared.txt"),
        "shared implementation\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "shared.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "prior reviewed implementation"],
    );
    let prior_reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    std::fs::write(
        workspace.path().join("shared.txt"),
        "shared implementation\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "shared.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "merge target already has implementation"],
    );

    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    for index in 0..47 {
        run_git(
            workspace.path(),
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("epic filler {index}"),
            ],
        );
    }
    let epic_base = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(
        workspace.path(),
        &["checkout", "worker/task-mixed-proof-retry"],
    );
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "follow-up review delta"],
    );
    let followup_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Mixed Proof Retry Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &followup_commit,
        &[&prior_reviewed_commit, &followup_commit],
    );

    let first_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(first_integrate.is_error, Some(true));
    let first_result: Value = serde_json::from_str(&extract_text(&first_integrate)).unwrap();
    assert_eq!(first_result["integration_phase"], "cherry_picking");
    assert_eq!(first_result["status"], "waiting_for_supervisor");
    assert_eq!(
        first_result["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    assert_eq!(
        first_result["next_action_for_supervisor"]["kind"],
        "resolve_and_continue"
    );
    assert_eq!(
        first_result["next_action_for_brehon"]["kind"],
        "detect_on_retry"
    );
    let integration_worktree = first_result["worktree_path"].as_str().unwrap();

    std::fs::write(
        Path::new(integration_worktree).join("src/conflict.txt"),
        "epic branch\nworker branch\n",
    )
    .unwrap();
    run_git(
        Path::new(integration_worktree),
        &["add", "src/conflict.txt"],
    );
    run_git(
        Path::new(integration_worktree),
        &["cherry-pick", "--continue"],
    );

    let second_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        second_integrate.is_error.is_none(),
        "{}",
        extract_text(&second_integrate)
    );
    let second_result: Value = serde_json::from_str(&extract_text(&second_integrate)).unwrap();
    assert_eq!(second_result["action"], "integrated");
    assert_eq!(second_result["integration_phase"], "complete");
    assert_eq!(second_result["status"], "integrated");
    assert_eq!(second_result["terminal_status"], "closed");
    assert_eq!(second_result["conflicting_files"], serde_json::json!([]));
    assert_eq!(second_result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(second_result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(second_result["reviewed_commit"], followup_commit);
    assert_eq!(
        second_result["reviewed_commits"],
        serde_json::json!([prior_reviewed_commit, followup_commit])
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
    assert_eq!(
        run_git(
            Path::new(integration_worktree),
            &["show", "HEAD:shared.txt"]
        ),
        "shared implementation"
    );
    assert_eq!(
        run_git(
            Path::new(integration_worktree),
            &["show", "HEAD:src/conflict.txt"],
        ),
        "epic branch\nworker branch"
    );
    let ahead_count = run_git(
        Path::new(integration_worktree),
        &["rev-list", "--count", &format!("{epic_base}..HEAD")],
    );
    assert_eq!(ahead_count, "1");
}

#[tokio::test]
async fn test_integrate_action_rejects_cleared_conflict_after_cherry_pick_abort() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    run_git(workspace.path(), &["checkout", "main"]);

    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-abort-retry"],
    );
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker branch change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Abort Retry Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&reviewed_commit],
    );

    let first_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();
    assert_eq!(first_integrate.is_error, Some(true));
    let first_result: Value = serde_json::from_str(&extract_text(&first_integrate)).unwrap();
    assert_eq!(first_result["integration_phase"], "cherry_picking");
    assert_eq!(first_result["status"], "waiting_for_supervisor");
    assert_eq!(
        first_result["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    assert_eq!(
        first_result["next_action_for_supervisor"]["kind"],
        "resolve_and_continue"
    );
    assert_eq!(
        first_result["next_action_for_brehon"]["kind"],
        "detect_on_retry"
    );
    let integration_worktree = first_result["worktree_path"].as_str().unwrap();

    run_git(Path::new(integration_worktree), &["cherry-pick", "--abort"]);

    let second_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(second_integrate.is_error, Some(true));
    let second_result: Value = serde_json::from_str(&extract_text(&second_integrate)).unwrap();
    assert_eq!(second_result["action"], "integrate");
    assert_eq!(second_result["status"], "error");
    assert_eq!(
        second_result["error_code"],
        "cleared_cherry_pick_not_applied"
    );
    assert_eq!(second_result["integration_phase"], "cherry_picking");
    assert_eq!(second_result["current_status"], "approved");
    assert_eq!(
        second_result["next_action_for_supervisor"]["kind"],
        "abort_or_resolve"
    );
    assert_eq!(second_result["next_action_for_brehon"]["kind"], "none");

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "approved");
    assert_eq!(stored["integration_status"], "pending");
    assert_eq!(stored["integration"]["phase"], "cherry_picking");
    assert_eq!(
        stored["integration"]["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
}

#[tokio::test]
async fn test_integrate_action_force_retries_from_aborted_state() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-force-aborted"],
    );
    std::fs::write(workspace.path().join("feature.txt"), "force retry\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Force Retry Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    task["integration"] = serde_json::json!({
        "phase": "aborted",
        "epic_branch": "epic/test-feature",
        "worktree_path": "",
        "reviewed_commits": [reviewed_commit],
        "started_at": "2026-04-23T00:00:00Z",
        "last_transition_at": "2026-04-23T00:01:00Z",
        "conflicting_files": ["feature.txt"],
        "attempts": 2,
        "resolution": {
            "kind": "manual_abort",
            "reason": "operator aborted previous attempt",
            "resolved_at": "2026-04-23T00:02:00Z"
        }
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata(&brehon_root, subtask_id, "approved", &reviewed_commit);

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id,
            "force": true
        }))
        .await
        .unwrap();

    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrated");
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(result["status"], "integrated");
    assert_eq!(result["conflicting_files"], serde_json::json!([]));
    assert_eq!(result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
    assert_eq!(stored["merged_branch"], "epic/test-feature");
}

#[tokio::test]
async fn test_abort_integration_requires_force_before_retrying_integrate() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    run_git(workspace.path(), &["checkout", "main"]);

    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-explicit-abort"],
    );
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker branch change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json =
        create_subtask_for_test(&tool, "Explicit Abort Retry Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&reviewed_commit],
    );

    let first_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(first_integrate.is_error, Some(true));
    let first_result: Value = serde_json::from_str(&extract_text(&first_integrate)).unwrap();
    assert_eq!(first_result["integration_phase"], "cherry_picking");
    assert_eq!(
        first_result["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    let integration_worktree = first_result["worktree_path"].as_str().unwrap();
    let cherry_pick_head = git_path_in(Path::new(integration_worktree), "CHERRY_PICK_HEAD");
    assert!(
        cherry_pick_head.exists(),
        "expected CHERRY_PICK_HEAD to exist"
    );

    let abort_result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": subtask_id,
            "reason": "operator aborted conflicting attempt"
        }))
        .await
        .unwrap();

    assert!(
        abort_result.is_error.is_none(),
        "{}",
        extract_text(&abort_result)
    );
    let abort_json: Value = serde_json::from_str(&extract_text(&abort_result)).unwrap();
    assert_eq!(abort_json["action"], "abort-integration");
    assert_eq!(abort_json["integration_phase"], "aborted");
    assert_eq!(abort_json["task_status"], "approved");
    assert_eq!(abort_json["reason"], "operator aborted conflicting attempt");
    assert_eq!(abort_json["cleanup_action"], "git cherry-pick --abort");
    assert_eq!(
        run_git(Path::new(integration_worktree), &["status", "--porcelain"]),
        ""
    );
    assert!(
        !cherry_pick_head.exists(),
        "expected CHERRY_PICK_HEAD to be cleared"
    );

    let aborted = read_test_task(&brehon_root, subtask_id);
    assert_eq!(aborted["status"], "approved");
    assert_eq!(aborted["integration_status"], "pending");
    assert_eq!(aborted["integration"]["phase"], "aborted");
    assert!(
        !aborted["integration"]["resolution"]["resolved_at"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "abort should persist integration.resolution.resolved_at"
    );

    let rejected_retry = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(rejected_retry.is_error, Some(true));
    let rejected_text = extract_text(&rejected_retry);
    assert!(
        rejected_text.contains("force=true"),
        "retry rejection should mention force=true: {rejected_text}"
    );

    let forced_retry = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id,
            "force": true
        }))
        .await
        .unwrap();

    assert_eq!(forced_retry.is_error, Some(true));
    let forced_json: Value = serde_json::from_str(&extract_text(&forced_retry)).unwrap();
    assert_eq!(forced_json["integration_phase"], "cherry_picking");
    assert_eq!(forced_json["status"], "waiting_for_supervisor");
    assert_eq!(
        forced_json["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    assert_eq!(
        forced_json["next_action_for_supervisor"]["kind"],
        "resolve_and_continue"
    );
    assert_eq!(
        forced_json["next_action_for_brehon"]["kind"],
        "detect_on_retry"
    );
    let retried_worktree = forced_json["worktree_path"].as_str().unwrap();

    std::fs::write(
        Path::new(retried_worktree).join("src/conflict.txt"),
        "epic branch\nworker branch\n",
    )
    .unwrap();
    run_git(Path::new(retried_worktree), &["add", "src/conflict.txt"]);
    run_git(Path::new(retried_worktree), &["cherry-pick", "--continue"]);

    let completed_retry = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        completed_retry.is_error.is_none(),
        "{}",
        extract_text(&completed_retry)
    );
    let completed_json: Value = serde_json::from_str(&extract_text(&completed_retry)).unwrap();
    assert_eq!(completed_json["action"], "integrated");
    assert_eq!(completed_json["integration_phase"], "complete");
    assert_eq!(completed_json["status"], "integrated");
    assert_eq!(completed_json["conflicting_files"], serde_json::json!([]));
    assert_eq!(completed_json["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(completed_json["next_action_for_brehon"]["kind"], "none");
    assert_eq!(
        completed_json["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
    assert_eq!(stored["integration"]["attempts"], 1);
    assert_eq!(
        stored["integration"]["conflicting_files"],
        serde_json::json!([])
    );
    assert!(
        stored["integration"]["resolution"].is_null(),
        "force retry should reset the aborted resolution before completing"
    );
}

#[tokio::test]
async fn test_integrate_action_force_retries_irrecoverable_cherry_pick_state_destructively() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-force-cherry-pick"],
    );
    std::fs::write(workspace.path().join("feature.txt"), "force cherry-pick\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "feature work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Force Reset Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let parent_task = read_test_task(&brehon_root, epic_id);
    let integration_worktree = ensure_epic_integration_worktree(
        epic_id,
        "epic/test-feature",
        parent_task
            .get("integration_worktree")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty()),
        false,
        false,
    )
    .await
    .unwrap();
    let scratch_path = integration_worktree.join("scratch.txt");
    std::fs::write(&scratch_path, "stale operator scratch\n").unwrap();
    assert!(scratch_path.exists());

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    task["integration"] = serde_json::json!({
        "phase": "cherry_picking",
        "epic_branch": "epic/test-feature",
        "worktree_path": integration_worktree.to_string_lossy().to_string(),
        "reviewed_commits": [reviewed_commit],
        "started_at": "2026-04-23T00:00:00Z",
        "last_transition_at": "2026-04-23T00:01:00Z",
        "conflicting_files": ["feature.txt"],
        "attempts": 1,
        "resolution": Value::Null
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata(&brehon_root, subtask_id, "approved", &reviewed_commit);

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id,
            "force": true
        }))
        .await
        .unwrap();

    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );
    assert!(
        !scratch_path.exists(),
        "force retry should destructively clean stale worktree state"
    );

    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrated");
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(result["status"], "integrated");
    assert_eq!(result["conflicting_files"], serde_json::json!([]));
    assert_eq!(result["next_action_for_supervisor"]["kind"], "none");
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");
    assert_eq!(
        result["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );
    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration"]["phase"], "complete");
    let integrated_head = run_git(
        Path::new(result["integration_worktree"].as_str().unwrap()),
        &["rev-parse", "HEAD"],
    );
    assert_eq!(stored["merged_commit"], integrated_head);
}

#[tokio::test]
async fn test_stale_worktree_rejection_can_be_cleaned_and_force_retried_from_null_state() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-reviewed"],
    );
    std::fs::write(workspace.path().join("feature.txt"), "reviewed change\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "reviewed work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-stale-seed"],
    );
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(
        workspace.path().join("src/conflict.txt"),
        "stale cherry-pick seed\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "stale cherry-pick seed"],
    );
    let stale_seed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Stale Worktree Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let parent_task = read_test_task(&brehon_root, epic_id);
    let integration_worktree = ensure_epic_integration_worktree(
        epic_id,
        "epic/test-feature",
        parent_task
            .get("integration_worktree")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty()),
        false,
        false,
    )
    .await
    .unwrap();

    let stale_error = run_git_expect_failure(
        &integration_worktree,
        &["cherry-pick", "-x", &stale_seed_commit],
    );
    assert!(
        stale_error.contains("conflict") || stale_error.contains("Merge conflict"),
        "unexpected stale cherry-pick seed failure: {stale_error}"
    );
    let stale_cherry_pick_head = git_path_in(&integration_worktree, "CHERRY_PICK_HEAD");
    assert!(
        stale_cherry_pick_head.exists(),
        "expected stale CHERRY_PICK_HEAD to exist"
    );

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&reviewed_commit],
    );

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(integrate_result.is_error, Some(true));
    let integrate_err = extract_text(&integrate_result);
    assert!(
        integrate_err.contains("stale cherry-pick"),
        "integrate should report stale cherry-pick state: {integrate_err}"
    );

    let stored_after_reject = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored_after_reject["status"], "approved");
    assert_eq!(stored_after_reject["integration_status"], "pending");
    assert!(
        stored_after_reject.get("integration").is_none(),
        "stale worktree rejection should leave integration state unchanged"
    );

    let abort_result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": subtask_id,
            "reason": "clear stale worktree before retry"
        }))
        .await
        .unwrap();

    assert!(
        abort_result.is_error.is_none(),
        "{}",
        extract_text(&abort_result)
    );
    let abort_json: Value = serde_json::from_str(&extract_text(&abort_result)).unwrap();
    assert_eq!(abort_json["action"], "abort-integration");
    assert_eq!(abort_json["integration_phase"], "null");
    assert_eq!(abort_json["task_status"], "approved");
    assert_eq!(abort_json["cleanup_action"], "git cherry-pick --abort");
    assert_eq!(abort_json["noop"], Value::Bool(false));
    assert_eq!(
        run_git(&integration_worktree, &["status", "--porcelain"]),
        ""
    );
    assert!(
        !stale_cherry_pick_head.exists(),
        "abort-integration should clear the stale CHERRY_PICK_HEAD"
    );

    let stored_after_abort = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored_after_abort["status"], "approved");
    assert_eq!(stored_after_abort["integration_status"], "pending");
    assert!(
        stored_after_abort.get("integration").is_none(),
        "cleanup of stale null-phase worktree should not invent integration state"
    );

    let retry_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id,
            "force": true
        }))
        .await
        .unwrap();

    assert!(
        retry_result.is_error.is_none(),
        "{}",
        extract_text(&retry_result)
    );
    let retry_json: Value = serde_json::from_str(&extract_text(&retry_result)).unwrap();
    assert_eq!(retry_json["action"], "integrated");
    assert_eq!(retry_json["integration_phase"], "complete");
    assert_eq!(retry_json["status"], "integrated");
    assert_eq!(
        retry_json["reviewed_commits"],
        serde_json::json!([reviewed_commit])
    );

    let stored_after_retry = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored_after_retry["status"], "closed");
    assert_eq!(stored_after_retry["integration_status"], "integrated");
    assert_eq!(stored_after_retry["integration"]["phase"], "complete");
}

#[tokio::test]
async fn test_integrate_action_force_rejects_completed_integration() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Completed Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "closed".into();
    task["integration_status"] = "integrated".into();
    task["merge_target"] = "epic/test-feature".into();
    task["merged_branch"] = "epic/test-feature".into();
    task["merged_commit"] = run_git(workspace.path(), &["rev-parse", "main"]).into();
    task["integration"] = serde_json::json!({
        "phase": "complete",
        "epic_branch": "epic/test-feature",
        "worktree_path": "",
        "reviewed_commits": [],
        "started_at": "2026-04-23T00:00:00Z",
        "last_transition_at": "2026-04-23T00:01:00Z",
        "conflicting_files": [],
        "attempts": 1,
        "resolution": Value::Null
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id,
            "force": true
        }))
        .await
        .unwrap();

    assert_eq!(integrate_result.is_error, Some(true));
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["action"], "integrate");
    assert_eq!(result["status"], "error");
    assert_eq!(result["error_code"], "integration_already_completed");
    assert_eq!(result["current_status"], "closed");
    assert_eq!(result["integration_phase"], "complete");
    assert_eq!(
        result["next_action_for_supervisor"]["kind"],
        "manual_revert_required"
    );
    assert_eq!(result["next_action_for_brehon"]["kind"], "none");
    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
}

#[tokio::test]
async fn test_integrate_action_rejects_non_supervisor_and_notifies_supervisor() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let workspace = tempfile::tempdir().unwrap();
    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "title": "Feature Epic",
            "description": "Integrate approved subtasks through a feature branch.",
            "acceptance_criteria": ["Epic branch exists", "Subtasks target the epic branch"],
            "plan_steps": ["Create epic", "Create subtask"],
            "integration_branch": "epic/test-feature"
        }))
        .await
        .unwrap();
    assert!(
        epic_result.is_error.is_none(),
        "{}",
        extract_text(&epic_result)
    );
    let epic: Value = serde_json::from_str(&extract_text(&epic_result)).unwrap();
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask_result = tool
            .execute(serde_json::json!({
                "action": "create",
                "title": "Epic Subtask",
                "parent_id": epic_id,
                "description": "Validate supervisor-only integration gating.",
                "acceptance_criteria": ["Workers cannot run integrate", "Supervisor receives a notification"],
                "file_hints": ["crates/brehon-mcp/src/tools/task_actions.rs"],
                "test_requirements": ["cargo test -p brehon-mcp --lib tools::task_actions::tests::test_integrate_action_rejects_non_supervisor_and_notifies_supervisor -- --exact"],
                "plan_steps": ["Attempt integrate as worker", "Verify rejection and notification"]
            }))
            .await
            .unwrap();
    assert!(
        subtask_result.is_error.is_none(),
        "{}",
        extract_text(&subtask_result)
    );
    let subtask: Value = serde_json::from_str(&extract_text(&subtask_result)).unwrap();
    let subtask_id = subtask["task_id"].as_str().unwrap();

    let mut task = read_test_task(root.path(), subtask_id);
    task["status"] = "approved".into();
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let error_json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(error_json["action"], "integrate");
    assert_eq!(error_json["status"], "error");
    assert_eq!(error_json["error_code"], "supervisor_only");
    assert_eq!(error_json["current_status"], "approved");
    assert_eq!(
        error_json["next_action_for_supervisor"]["kind"],
        "notify_supervisor"
    );
    assert_eq!(error_json["next_action_for_brehon"]["kind"], "none");

    let expected_notification = format!(
        "Task {subtask_id} (\"Epic Subtask\") requires epic-branch integration after approval. \
         worker-1 attempted task action=integrate, but only supervisors can perform \
         post-review integration. Please run:\n  \
         task action=integrate id={subtask_id}"
    );
    let queued = read_queued_prompts(root.path());
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0]["target"], "sup-1");
    assert_eq!(queued[0]["from"], "worker-1");
    assert_eq!(queued[0]["message"], expected_notification);
}

#[tokio::test]
async fn test_list_output_includes_merge_target_for_subtasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    // Create feature epic
    let epic = create_epic_for_test(&tool, "Feature Epic", Some("epic/test")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    let subtask = create_subtask_for_test(&tool, "Subtask", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    // List should include merge_target for the subtask
    let list_result = tool
        .execute(serde_json::json!({
            "action": "list"
        }))
        .await
        .unwrap();

    let list: Value = serde_json::from_str(&extract_text(&list_result)).unwrap();
    let tasks = list["tasks"].as_array().unwrap();

    let found_subtask = tasks.iter().find(|t| t["task_id"] == subtask_id).unwrap();
    assert_eq!(found_subtask["merge_target"], "epic/test");
    assert_eq!(found_subtask["integration_status"], "pending");
}

#[tokio::test]
async fn test_epic_list_shows_integration_progress() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    // Create feature epic
    let epic = create_epic_for_test(&tool, "Feature Epic", Some("epic/test")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    // Create two subtasks
    let _ = create_subtask_for_test(&tool, "Subtask 1", epic_id).await;
    let subtask2 = create_subtask_for_test(&tool, "Subtask 2", epic_id).await;
    let subtask2_id = subtask2["task_id"].as_str().unwrap();

    // Mark one as integrated
    let mut task = read_test_task(root.path(), subtask2_id);
    task["integration_status"] = "integrated".into();
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask2_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    // List epics
    let list_result = tool
        .execute(serde_json::json!({
            "action": "list",
            "task_type": "epic"
        }))
        .await
        .unwrap();

    let list: Value = serde_json::from_str(&extract_text(&list_result)).unwrap();
    let epics = list["tasks"].as_array().unwrap();
    let found_epic = epics.iter().find(|t| t["task_id"] == epic_id).unwrap();

    // Should show integration progress
    assert_eq!(found_epic["integration_progress"]["integrated"], 1);
    assert_eq!(found_epic["integration_progress"]["total"], 2);
}

#[tokio::test]
async fn test_feature_epic_close_merges_branch_to_main() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    // Setup git workspace
    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::write(workspace.path().join("epic-change.txt"), "epic work\n").unwrap();
    run_git(workspace.path(), &["add", "epic-change.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic work"]);
    let epic_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    // Create feature epic
    let epic = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic["task_id"].as_str().unwrap();

    // Create subtask and mark integrated
    let subtask = create_subtask_for_test(&tool, "Subtask 1", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    // Close subtask and mark integrated
    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "closed".into();
    task["integration_status"] = "integrated".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    // Close epic - should trigger branch merge
    let close_result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();

    assert!(
        close_result.is_error.is_none(),
        "epic close should succeed: {}",
        extract_text(&close_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&close_result)).unwrap();
    assert_eq!(result["action"], "merged");
    assert_eq!(result["integration_branch"], "epic/test-feature");
    assert_eq!(result["merge_strategy"], "merge");
    assert!(
        result.get("merged_commit").is_some(),
        "should have merged_commit"
    );
    assert!(
        result.get("merged_branch").is_some(),
        "should have merged_branch"
    );

    // Verify stored task has terminal lifecycle fields
    let stored_epic = read_test_task(&brehon_root, epic_id);
    assert_eq!(
        stored_epic["status"], "merged",
        "epic status should be 'merged'"
    );
    assert_eq!(stored_epic["epic_branch_merged"], "epic/test-feature");
    assert_eq!(stored_epic["merge_strategy"], "merge");
    assert!(
        stored_epic.get("squash_source_tip").is_none(),
        "top-level epic direct-to-main close should not use squash metadata"
    );
    assert!(
        stored_epic.get("closed_at").is_some(),
        "epic should have closed_at"
    );
    assert!(
        stored_epic.get("updated_at").is_some(),
        "epic should have updated_at"
    );
    assert!(
        stored_epic.get("closed_by").is_some(),
        "epic should have closed_by"
    );
    assert!(
        stored_epic.get("merged_commit").is_some(),
        "epic should have merged_commit"
    );

    // Verify the merge happened
    let after_merge_head = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    assert_ne!(
        after_merge_head, epic_commit,
        "HEAD should have moved after merge"
    );

    // Verify epic branch is ancestor of main now
    let is_ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", &epic_commit, "main"])
        .current_dir(workspace.path())
        .status()
        .unwrap();
    assert!(
        is_ancestor.success(),
        "epic commit should be ancestor of main after merge"
    );
}

#[tokio::test]
async fn test_child_epic_close_integrates_into_initiative_branch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "Program").await;
    let initiative_id = initiative["task_id"].as_str().unwrap();
    let initiative_branch = initiative["integration_branch"].as_str().unwrap();
    let initiative_worktree = PathBuf::from(initiative["integration_worktree"].as_str().unwrap());

    let epic_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "parent_id": initiative_id,
            "title": "Phase 1",
            "description": "Phase 1 delivers the first implementation slice.",
            "acceptance_criteria": ["Phase 1 closes with all worker tasks complete"],
            "plan_steps": ["Create tasks", "Run review", "Close phase"]
        }))
        .await
        .unwrap();
    assert!(
        epic_result.is_error.is_none(),
        "{}",
        extract_text(&epic_result)
    );
    let epic: Value = serde_json::from_str(&extract_text(&epic_result)).unwrap();
    let epic_id = epic["task_id"].as_str().unwrap();
    let epic_branch = epic["integration_branch"].as_str().unwrap();
    let epic_worktree = PathBuf::from(epic["integration_worktree"].as_str().unwrap());

    std::fs::write(epic_worktree.join("phase1.txt"), "phase 1\n").unwrap();
    run_git(&epic_worktree, &["add", "phase1.txt"]);
    run_git(&epic_worktree, &["commit", "-m", "phase 1 implementation"]);
    let epic_commit = run_git(&epic_worktree, &["rev-parse", "HEAD"]);

    let subtask = create_subtask_for_test(&tool, "Subtask 1", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();
    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "closed".into();
    task["integration_status"] = "integrated".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let close_result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();

    assert!(
        close_result.is_error.is_none(),
        "{}",
        extract_text(&close_result)
    );
    let result: Value = serde_json::from_str(&extract_text(&close_result)).unwrap();
    assert_eq!(result["action"], "closed");
    assert_eq!(result["merged_branch"], initiative_branch);
    assert_eq!(result["integration_status"], "integrated");
    assert_eq!(result["merge_strategy"], "merge");

    let stored_epic = read_test_task(&brehon_root, epic_id);
    assert_eq!(stored_epic["status"], "closed");
    assert_eq!(stored_epic["integration_status"], "integrated");
    assert_eq!(stored_epic["merged_branch"], initiative_branch);
    assert_eq!(stored_epic["merge_strategy"], "merge");
    assert!(
        stored_epic.get("squash_source_tip").is_none(),
        "child epic close into initiative branch should not use squash metadata"
    );

    assert!(initiative_worktree.join("phase1.txt").exists());
    let is_ancestor = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            &epic_commit,
            initiative_branch,
        ])
        .current_dir(workspace.path())
        .status()
        .unwrap();
    assert!(
        is_ancestor.success(),
        "epic commit should be ancestor of initiative branch after close"
    );
    let not_on_main = Command::new("git")
        .args(["merge-base", "--is-ancestor", &epic_commit, "main"])
        .current_dir(workspace.path())
        .status()
        .unwrap();
    assert!(
        !not_on_main.success(),
        "epic commit should not reach main until the initiative closes"
    );
    assert_eq!(
        run_git(
            Path::new(epic_worktree.as_path()),
            &["branch", "--show-current"]
        ),
        epic_branch
    );
}

#[tokio::test]
async fn test_initiative_close_merges_initiative_branch_to_main() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "Program").await;
    let initiative_id = initiative["task_id"].as_str().unwrap();
    let initiative_branch = initiative["integration_branch"].as_str().unwrap();

    let epic_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "parent_id": initiative_id,
            "title": "Phase 1",
            "description": "Phase 1 delivers the first implementation slice.",
            "acceptance_criteria": ["Phase 1 closes with all worker tasks complete"],
            "plan_steps": ["Create tasks", "Run review", "Close phase"]
        }))
        .await
        .unwrap();
    assert!(
        epic_result.is_error.is_none(),
        "{}",
        extract_text(&epic_result)
    );
    let epic: Value = serde_json::from_str(&extract_text(&epic_result)).unwrap();
    let epic_id = epic["task_id"].as_str().unwrap();
    let epic_worktree = PathBuf::from(epic["integration_worktree"].as_str().unwrap());

    std::fs::write(epic_worktree.join("phase1.txt"), "phase 1\n").unwrap();
    run_git(&epic_worktree, &["add", "phase1.txt"]);
    run_git(&epic_worktree, &["commit", "-m", "phase 1 implementation"]);
    let epic_commit = run_git(&epic_worktree, &["rev-parse", "HEAD"]);

    let subtask = create_subtask_for_test(&tool, "Subtask 1", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();
    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "closed".into();
    task["integration_status"] = "integrated".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    // Supervisor close runs `verify_merge_ready` which requires HEAD on
    // merge_target; init_git_workspace leaves us on `worker/test`.
    run_git(workspace.path(), &["checkout", "main"]);

    let epic_close = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();
    assert!(
        epic_close.is_error.is_none(),
        "{}",
        extract_text(&epic_close)
    );
    let squash_source_tip = run_git(workspace.path(), &["rev-parse", initiative_branch]);

    let initiative_close = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": initiative_id
        }))
        .await
        .unwrap();
    assert!(
        initiative_close.is_error.is_none(),
        "{}",
        extract_text(&initiative_close)
    );
    let result: Value = serde_json::from_str(&extract_text(&initiative_close)).unwrap();
    assert_eq!(result["action"], "merged");
    assert_eq!(result["merged_branch"], "main");
    assert_eq!(result["merge_strategy"], "squash_with_lineage");
    assert_eq!(result["squash_source_tip"], squash_source_tip);

    let stored_initiative = read_test_task(&brehon_root, initiative_id);
    assert_eq!(stored_initiative["status"], "merged");
    assert_eq!(stored_initiative["merged_branch"], "main");
    assert_eq!(stored_initiative["merge_strategy"], "squash_with_lineage");
    assert_eq!(stored_initiative["squash_source_tip"], squash_source_tip);
    assert_eq!(
        run_git(workspace.path(), &["rev-parse", "main"]),
        result["merged_commit"].as_str().unwrap()
    );
    assert_eq!(
        stored_initiative["merged_commit"], result["merged_commit"],
        "stored merged_commit should be the final squash commit"
    );
    assert_eq!(
        run_git(workspace.path(), &["log", "-1", "--format=%s"]),
        format!("Merge initiative {initiative_id} lineage: Program")
    );
    assert_eq!(
        run_git(workspace.path(), &["log", "-2", "--format=%s"])
            .lines()
            .nth(1)
            .unwrap(),
        format!("Merge initiative {initiative_id}: Program")
    );

    let on_main = Command::new("git")
        .args(["merge-base", "--is-ancestor", &epic_commit, "main"])
        .current_dir(workspace.path())
        .status()
        .unwrap();
    assert!(
        on_main.success(),
        "lineage close should make child epic commits reachable through the initiative branch"
    );
    let initiative_branch_is_ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", initiative_branch, "main"])
        .current_dir(workspace.path())
        .status()
        .unwrap();
    assert!(
        initiative_branch_is_ancestor.success(),
        "initiative close should record lineage so git sees the source branch as merged"
    );
    assert!(
        run_git(workspace.path(), &["diff", "--stat", "HEAD^1", "HEAD"]).is_empty(),
        "lineage commit should not change the squashed content tree"
    );
    let initiative_tree_spec = format!("{initiative_branch}^{{tree}}");
    assert_eq!(
        run_git(workspace.path(), &["rev-parse", "main^{tree}"]),
        run_git(workspace.path(), &["rev-parse", &initiative_tree_spec]),
        "main content tree should match the initiative branch after squash"
    );
}

#[tokio::test]
async fn test_initiative_squash_close_rejects_dirty_default_branch_worktree_without_task_update() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = create_initiative_for_test(&tool, "Program").await;
    let initiative_id = initiative["task_id"].as_str().unwrap();

    let epic_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "task_type": "epic",
            "parent_id": initiative_id,
            "title": "Phase 1",
            "description": "Phase 1 delivers the first implementation slice.",
            "acceptance_criteria": ["Phase 1 closes with all worker tasks complete"],
            "plan_steps": ["Create tasks", "Run review", "Close phase"]
        }))
        .await
        .unwrap();
    assert!(
        epic_result.is_error.is_none(),
        "{}",
        extract_text(&epic_result)
    );
    let epic: Value = serde_json::from_str(&extract_text(&epic_result)).unwrap();
    let epic_id = epic["task_id"].as_str().unwrap();
    let epic_worktree = PathBuf::from(epic["integration_worktree"].as_str().unwrap());

    std::fs::write(epic_worktree.join("phase1.txt"), "phase 1\n").unwrap();
    run_git(&epic_worktree, &["add", "phase1.txt"]);
    run_git(&epic_worktree, &["commit", "-m", "phase 1 implementation"]);

    let subtask = create_subtask_for_test(&tool, "Subtask 1", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();
    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "closed".into();
    task["integration_status"] = "integrated".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    run_git(workspace.path(), &["checkout", "main"]);

    let epic_close = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();
    assert!(
        epic_close.is_error.is_none(),
        "{}",
        extract_text(&epic_close)
    );

    let stored_before = read_test_task(&brehon_root, initiative_id);
    std::fs::write(
        workspace.path().join("README.md"),
        "dirty target worktree\n",
    )
    .unwrap();

    let initiative_close = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": initiative_id
        }))
        .await
        .unwrap();

    assert!(
        initiative_close.is_error.is_some(),
        "dirty target worktree should block final squash close"
    );
    let text = extract_text(&initiative_close);
    assert!(
        text.contains("shared repo root") && text.contains("dirty"),
        "expected shared-root dirty error, got: {text}"
    );
    let stored_after = read_test_task(&brehon_root, initiative_id);
    assert_eq!(
        stored_after, stored_before,
        "failed squash close must leave initiative task JSON unchanged"
    );
}

#[tokio::test]
async fn test_promote_followups_creates_child_task_and_marks_followups_tasked() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic = create_epic_for_test(&tool, "Phase 1", Some("epic/test-followups")).await;
    let epic_id = epic["task_id"].as_str().unwrap();
    let subtask = create_subtask_for_test(&tool, "Implement queue repair", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["review_followups"] = serde_json::json!([
        {
            "followup_id": "FUP-1",
            "status": "open",
            "severity": "suggestion",
            "description": "Split the helper into transport and policy concerns",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        },
        {
            "followup_id": "FUP-2",
            "status": "open",
            "severity": "nitpick",
            "description": "Add one more regression test around queue cleanup",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        }
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let promote = tool
        .execute(serde_json::json!({
            "action": "promote_followups",
            "id": subtask_id
        }))
        .await
        .unwrap();
    assert!(promote.is_error.is_none(), "{}", extract_text(&promote));
    let payload: Value = serde_json::from_str(&extract_text(&promote)).unwrap();
    let followup_task_id = payload["followup_task_id"].as_str().unwrap();

    let updated_source = read_test_task(&brehon_root, subtask_id);
    let source_followups = updated_source["review_followups"].as_array().unwrap();
    assert_eq!(source_followups.len(), 2);
    assert!(source_followups
        .iter()
        .all(|followup| followup["status"] == "tasked"));
    assert!(source_followups
        .iter()
        .all(|followup| { followup["followup_task_id"] == followup_task_id }));

    let promoted_task = read_test_task(&brehon_root, followup_task_id);
    assert_eq!(promoted_task["status"], "pending");
    assert_eq!(promoted_task["parent_id"], epic_id);
    assert_eq!(promoted_task["merge_target"], "epic/test-followups");
    assert_eq!(promoted_task["source_task_id"], subtask_id);
}

#[tokio::test]
async fn test_close_marks_promoted_followups_done_on_source_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic = create_plain_epic_for_test(&tool, "Audit Epic").await;
    let epic_id = epic["task_id"].as_str().unwrap();
    let source_result = tool
        .execute(serde_json::json!({
            "action": "create",
            "title": "Audit queue cleanup",
            "parent_id": epic_id,
            "completion_mode": "close",
            "description": "Audit-only task with no code changes.",
            "acceptance_criteria": ["Track approved followups", "Close without merge"],
            "plan_steps": ["Inspect findings", "Track cleanup", "Close the audit task"]
        }))
        .await
        .unwrap();
    assert!(
        source_result.is_error.is_none(),
        "{}",
        extract_text(&source_result)
    );
    let source: Value = serde_json::from_str(&extract_text(&source_result)).unwrap();
    let source_id = source["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, source_id);
    task["review_followups"] = serde_json::json!([
        {
            "followup_id": "FUP-1",
            "status": "open",
            "severity": "suggestion",
            "description": "Split transport and policy concerns",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        },
        {
            "followup_id": "FUP-2",
            "status": "open",
            "severity": "nitpick",
            "description": "Add a regression test for stale cleanup",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        }
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", source_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let promote = tool
        .execute(serde_json::json!({
            "action": "promote_followups",
            "id": source_id
        }))
        .await
        .unwrap();
    assert!(promote.is_error.is_none(), "{}", extract_text(&promote));
    let payload: Value = serde_json::from_str(&extract_text(&promote)).unwrap();
    let followup_task_id = payload["followup_task_id"].as_str().unwrap();

    let mut promoted_task = read_test_task(&brehon_root, followup_task_id);
    promoted_task["status"] = "approved".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", followup_task_id)),
        serde_json::to_string_pretty(&promoted_task).unwrap(),
    )
    .unwrap();

    let close_result = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": followup_task_id
        }))
        .await
        .unwrap();
    assert!(
        close_result.is_error.is_none(),
        "{}",
        extract_text(&close_result)
    );

    let updated_source = read_test_task(&brehon_root, source_id);
    let followups = updated_source["review_followups"].as_array().unwrap();
    assert_eq!(followups.len(), 2);
    assert!(followups
        .iter()
        .all(|followup| followup["status"] == "done"));
    assert!(followups
        .iter()
        .all(|followup| followup.get("resolved_at").is_some()));
}

#[tokio::test]
async fn test_integrate_marks_promoted_followups_done_on_source_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["checkout", "-b", "epic/test-followup-integration"],
    );
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/followup-task"],
    );
    std::fs::write(workspace.path().join("followup.txt"), "followup work\n").unwrap();
    run_git(workspace.path(), &["add", "followup.txt"]);
    run_git(workspace.path(), &["commit", "-m", "followup work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic = create_epic_for_test(
        &tool,
        "Feature Epic",
        Some("epic/test-followup-integration"),
    )
    .await;
    let epic_id = epic["task_id"].as_str().unwrap();
    let source = create_subtask_for_test(&tool, "Implement source task", epic_id).await;
    let source_id = source["task_id"].as_str().unwrap();

    let mut source_task = read_test_task(&brehon_root, source_id);
    source_task["review_followups"] = serde_json::json!([
        {
            "followup_id": "FUP-1",
            "status": "open",
            "severity": "suggestion",
            "description": "Extract cleanup into a helper",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        }
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", source_id)),
        serde_json::to_string_pretty(&source_task).unwrap(),
    )
    .unwrap();

    let promote = tool
        .execute(serde_json::json!({
            "action": "promote_followups",
            "id": source_id
        }))
        .await
        .unwrap();
    assert!(promote.is_error.is_none(), "{}", extract_text(&promote));
    let payload: Value = serde_json::from_str(&extract_text(&promote)).unwrap();
    let followup_task_id = payload["followup_task_id"].as_str().unwrap();

    let mut followup_task = read_test_task(&brehon_root, followup_task_id);
    followup_task["status"] = "approved".into();
    followup_task["integration_status"] = "pending".into();
    followup_task["assignee"] = "worker-1".into();
    followup_task["review_owner"] = "worker-1".into();
    followup_task["activity"] = "testing".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", followup_task_id)),
        serde_json::to_string_pretty(&followup_task).unwrap(),
    )
    .unwrap();
    write_review_metadata(&brehon_root, followup_task_id, "approved", &reviewed_commit);

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": followup_task_id
        }))
        .await
        .unwrap();
    assert!(
        integrate_result.is_error.is_none(),
        "{}",
        extract_text(&integrate_result)
    );

    let updated_source = read_test_task(&brehon_root, source_id);
    let followups = updated_source["review_followups"].as_array().unwrap();
    assert_eq!(followups.len(), 1);
    assert_eq!(followups[0]["status"], "done");
    assert_eq!(followups[0]["followup_task_id"], followup_task_id);
    assert!(followups[0].get("resolved_at").is_some());
}

#[tokio::test]
async fn test_epic_close_rejects_open_followups_until_waived() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic = create_epic_for_test(&tool, "Phase 2", Some("epic/test-close-gate")).await;
    let epic_id = epic["task_id"].as_str().unwrap();
    let subtask = create_subtask_for_test(&tool, "Implement adapter", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "closed".into();
    task["integration_status"] = "integrated".into();
    task["review_followups"] = serde_json::json!([
        {
            "followup_id": "FUP-open",
            "status": "open",
            "severity": "suggestion",
            "description": "Split the adapter validation pass",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        }
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    // Supervisor close runs `verify_merge_ready` which requires HEAD on
    // merge_target; init_git_workspace leaves us on `worker/test`.
    run_git(workspace.path(), &["checkout", "main"]);

    let close_blocked = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();
    assert!(close_blocked.is_error.is_some());
    assert!(extract_text(&close_blocked).contains("unresolved approved-review followups"));

    let waive = tool
        .execute(serde_json::json!({
            "action": "waive_followups",
            "id": subtask_id,
            "reason": "Tracked separately by design note"
        }))
        .await
        .unwrap();
    assert!(waive.is_error.is_none(), "{}", extract_text(&waive));

    let close_allowed = tool
        .execute(serde_json::json!({
            "action": "close",
            "id": epic_id
        }))
        .await
        .unwrap();
    assert!(
        close_allowed.is_error.is_none(),
        "{}",
        extract_text(&close_allowed)
    );
}

#[tokio::test]
async fn test_waive_followups_requires_explicit_ids_or_waive_all_for_multiple_open_items() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    init_git_workspace(workspace.path());
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic = create_epic_for_test(&tool, "Phase Followups", Some("epic/test-waive-all")).await;
    let epic_id = epic["task_id"].as_str().unwrap();
    let subtask = create_subtask_for_test(&tool, "Implement something", epic_id).await;
    let subtask_id = subtask["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["review_followups"] = serde_json::json!([
        {
            "followup_id": "FUP-1",
            "status": "open",
            "severity": "suggestion",
            "description": "Do substantial cleanup",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        },
        {
            "followup_id": "FUP-2",
            "status": "open",
            "severity": "nitpick",
            "description": "Tighten a minor edge case",
            "created_at": "2026-04-10T00:00:00Z",
            "updated_at": "2026-04-10T00:00:00Z"
        }
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let blanket_waive = tool
        .execute(serde_json::json!({
            "action": "waive_followups",
            "id": subtask_id,
            "reason": "not important"
        }))
        .await
        .unwrap();
    assert!(blanket_waive.is_error.is_some());
    assert!(
        extract_text(&blanket_waive).contains("Do not blanket-waive them by default"),
        "{}",
        extract_text(&blanket_waive)
    );

    let selective_waive = tool
        .execute(serde_json::json!({
            "action": "waive_followups",
            "id": subtask_id,
            "followup_ids": ["FUP-2"],
            "reason": "No action needed for this nitpick"
        }))
        .await
        .unwrap();
    assert!(
        selective_waive.is_error.is_none(),
        "{}",
        extract_text(&selective_waive)
    );

    let updated = read_test_task(&brehon_root, subtask_id);
    let followups = updated["review_followups"].as_array().unwrap();
    assert_eq!(
        followups
            .iter()
            .find(|f| f["followup_id"] == "FUP-1")
            .unwrap()["status"],
        "open"
    );
    assert_eq!(
        followups
            .iter()
            .find(|f| f["followup_id"] == "FUP-2")
            .unwrap()["status"],
        "waived"
    );
}

#[tokio::test]
async fn test_abort_integration_aborts_cherry_pick_and_restores_approved_state() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    run_git(workspace.path(), &["checkout", "main"]);

    run_git(workspace.path(), &["checkout", "-b", "worker/task-abort"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker branch change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let integration_worktree = epic_json["integration_worktree"]
        .as_str()
        .unwrap()
        .to_string();
    let subtask_json = create_subtask_for_test(&tool, "Abort Integration Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let cherry_pick_error = run_git_expect_failure(
        Path::new(&integration_worktree),
        &["cherry-pick", "-x", &reviewed_commit],
    );
    assert!(
        cherry_pick_error.contains("conflict") || cherry_pick_error.contains("Merge conflict"),
        "unexpected cherry-pick failure: {cherry_pick_error}"
    );
    let cherry_pick_head = git_path_in(Path::new(&integration_worktree), "CHERRY_PICK_HEAD");
    assert!(
        cherry_pick_head.exists(),
        "expected CHERRY_PICK_HEAD to exist"
    );

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "changes_requested".into();
    task["integration_status"] = "pending".into();
    task["integration"] = serde_json::json!({
        "phase": "cherry_picking",
        "epic_branch": "epic/test-feature",
        "worktree_path": integration_worktree,
        "conflicting_files": ["src/conflict.txt"]
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": subtask_id,
            "reason": "manual stop"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["action"], "abort-integration");
    assert_eq!(json["integration_phase"], "aborted");
    assert_eq!(json["task_status"], "approved");
    assert_eq!(json["cleanup_action"], "git cherry-pick --abort");
    assert_eq!(json["reason"], "manual stop");

    assert_eq!(
        run_git(Path::new(&integration_worktree), &["status", "--porcelain"]),
        ""
    );
    assert!(
        !cherry_pick_head.exists(),
        "expected CHERRY_PICK_HEAD to be cleared"
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "approved");
    assert_eq!(stored["integration_status"], "pending");
    assert_eq!(stored["integration"]["phase"], "aborted");
    assert_eq!(stored["integration"]["resolution"]["kind"], "manual_abort");
    assert_eq!(stored["integration"]["resolution"]["reason"], "manual stop");
    assert_eq!(stored["integration"]["epic_branch"], "epic/test-feature");
}

#[tokio::test]
async fn test_abort_integration_resets_dirty_resolved_worktree_to_epic_tip() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let integration_worktree = epic_json["integration_worktree"]
        .as_str()
        .unwrap()
        .to_string();
    let subtask_json = create_subtask_for_test(&tool, "Reset Integration Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();
    let epic_branch_tip = run_git(Path::new(&integration_worktree), &["rev-parse", "HEAD"]);

    std::fs::write(
        Path::new(&integration_worktree).join("README.md"),
        "dirty resolve\n",
    )
    .unwrap();
    assert!(
        !run_git(Path::new(&integration_worktree), &["status", "--porcelain"]).is_empty(),
        "expected integration worktree to be dirty before abort"
    );

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "changes_requested".into();
    task["integration_status"] = "pending".into();
    task["integration"] = serde_json::json!({
        "phase": "resolved",
        "epic_branch": "epic/test-feature",
        "worktree_path": integration_worktree
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": subtask_id,
            "reason": "discard dirty resolution"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["action"], "abort-integration");
    assert_eq!(json["integration_phase"], "aborted");
    assert_eq!(json["task_status"], "approved");
    assert_eq!(
        json["cleanup_action"],
        Value::String(format!("git reset --hard {epic_branch_tip}"))
    );
    assert_eq!(
        run_git(Path::new(&integration_worktree), &["status", "--porcelain"]),
        ""
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&integration_worktree).join("README.md")).unwrap(),
        "seed\n"
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "approved");
    assert_eq!(stored["integration"]["phase"], "aborted");
    assert_eq!(stored["integration"]["resolution"]["kind"], "manual_abort");
    assert_eq!(
        stored["integration"]["resolution"]["reason"],
        "discard dirty resolution"
    );
}

#[tokio::test]
async fn test_abort_integration_noops_when_phase_is_null() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Null Phase Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": subtask_id,
            "reason": "noop"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["action"], "abort-integration");
    assert_eq!(json["noop"], Value::Bool(true));
    assert_eq!(json["integration_phase"], "null");
    assert_eq!(json["task_status"], "approved");

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "approved");
    assert!(stored.get("integration").is_none());
}

#[tokio::test]
async fn test_abort_integration_rejects_worker_role() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_NAME", "worker-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(&brehon_root, "T-abort-auth", "approved", "task");
    let mut task = read_test_task(&brehon_root, "T-abort-auth");
    task["integration"] = serde_json::json!({
        "phase": "cherry_picking",
        "epic_branch": "epic/test",
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-abort-auth.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": "T-abort-auth",
            "reason": "should fail"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result)
        .contains("Only supervisors can abort integration in an epic worktree."));

    let saw_notification = read_queued_prompts(&brehon_root)
        .into_iter()
        .any(|payload| {
            payload["target"] == "sup-1"
                && payload["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("task action=abort-integration id=T-abort-auth")
        });
    assert!(
        saw_notification,
        "supervisor should be notified about unauthorized abort-integration attempt"
    );
}

#[tokio::test]
async fn test_abort_integration_requires_reason() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(&brehon_root, "T-abort-reason", "approved", "task");
    let mut task = read_test_task(&brehon_root, "T-abort-reason");
    task["integration"] = serde_json::json!({
        "phase": "cherry_picking",
        "epic_branch": "epic/test",
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-abort-reason.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": "T-abort-reason"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("Missing required parameter: reason"));
}

#[tokio::test]
async fn test_abort_integration_rejects_unsupported_phase() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(&brehon_root, "T-abort-phase", "approved", "task");
    let mut task = read_test_task(&brehon_root, "T-abort-phase");
    task["integration"] = serde_json::json!({
        "phase": "pending_integration",
        "epic_branch": "epic/test",
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-abort-phase.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": "T-abort-phase",
            "reason": "unsupported phase"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("unsupported integration phase"));
    assert!(text.contains("pending_integration"));
}

#[tokio::test]
async fn test_abort_integration_noops_when_phase_is_complete() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(&brehon_root, "T-abort-complete", "closed", "task");
    let mut task = read_test_task(&brehon_root, "T-abort-complete");
    task["integration"] = serde_json::json!({
        "phase": "complete",
        "epic_branch": "epic/test",
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-abort-complete.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": "T-abort-complete",
            "reason": "noop"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["action"], "abort-integration");
    assert_eq!(json["noop"], Value::Bool(true));
    assert_eq!(json["integration_phase"], "complete");
    assert_eq!(json["task_status"], "closed");

    let stored = read_test_task(&brehon_root, "T-abort-complete");
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration"]["phase"], "complete");
}

#[tokio::test]
async fn test_abort_integration_noops_when_phase_is_aborted() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(&brehon_root, "T-abort-aborted", "approved", "task");
    let mut task = read_test_task(&brehon_root, "T-abort-aborted");
    task["integration"] = serde_json::json!({
        "phase": "aborted",
        "epic_branch": "epic/test",
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-abort-aborted.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "abort-integration",
            "id": "T-abort-aborted",
            "reason": "noop"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(json["action"], "abort-integration");
    assert_eq!(json["noop"], Value::Bool(true));
    assert_eq!(json["integration_phase"], "aborted");
    assert_eq!(json["task_status"], "approved");

    let stored = read_test_task(&brehon_root, "T-abort-aborted");
    assert_eq!(stored["status"], "approved");
    assert_eq!(stored["integration"]["phase"], "aborted");
}

#[tokio::test]
async fn test_integration_detects_patch_equivalent_after_rebase() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());

    // Epic branch adds src/conflict.txt with epic content, plus filler commits
    // so the branch is long enough for the 50-commit patch-equivalence window.
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "epic branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);
    for index in 0..50 {
        run_git(
            workspace.path(),
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("epic filler {index}"),
            ],
        );
    }
    run_git(workspace.path(), &["checkout", "main"]);

    // Worker branch adds the same file with different content
    run_git(workspace.path(), &["checkout", "-b", "worker/task-rebase"]);
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/conflict.txt"), "worker branch\n").unwrap();
    run_git(workspace.path(), &["add", "src/conflict.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker branch change"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);

    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
        ("BREHON_AGENT_NAME", "sup-1"),
        ("BREHON_SUPERVISOR_NAME", "sup-1"),
    ]);
    let tool = TaskActionsTool::new();

    let epic_json = create_epic_for_test(&tool, "Feature Epic", Some("epic/test-feature")).await;
    let epic_id = epic_json["task_id"].as_str().unwrap();
    let subtask_json = create_subtask_for_test(&tool, "Rebase Workaround Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "pending".into();
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join(format!("{}.json", subtask_id)),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
    write_review_metadata_with_commits(
        &brehon_root,
        subtask_id,
        "approved",
        &reviewed_commit,
        &[&reviewed_commit],
    );

    // Step 1: First integrate → conflict
    let first_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(first_integrate.is_error, Some(true));
    let first_result: Value = serde_json::from_str(&extract_text(&first_integrate)).unwrap();
    assert_eq!(first_result["integration_phase"], "cherry_picking");
    assert_eq!(first_result["status"], "waiting_for_supervisor");
    assert_eq!(
        first_result["conflicting_files"],
        serde_json::json!(["src/conflict.txt"])
    );
    let integration_worktree = first_result["worktree_path"].as_str().unwrap();

    // Step 2: Test harness "rebases" worker commits onto epic tip.
    // 2a. Abort the failed cherry-pick in the integration worktree.
    run_git(Path::new(integration_worktree), &["cherry-pick", "--abort"]);

    // 2b. In the integration worktree, revert epic's addition so the file
    //     no longer exists.
    run_git(Path::new(integration_worktree), &["rm", "src/conflict.txt"]);
    run_git(
        Path::new(integration_worktree),
        &["commit", "-m", "revert epic addition"],
    );

    // 2c. Cherry-pick the original worker commit onto epic in the worktree
    //     (applies cleanly, producing a commit with the same patch but a
    //     different SHA).
    run_git(
        Path::new(integration_worktree),
        &["cherry-pick", &reviewed_commit],
    );
    let rebased_commit = run_git(Path::new(integration_worktree), &["rev-parse", "HEAD"]);
    assert_ne!(
        reviewed_commit, rebased_commit,
        "rebased commit must have a different SHA"
    );

    // Step 3: Verify patch-equivalence directly.
    assert!(
        is_patch_equivalent_in_window_in(
            Path::new(integration_worktree),
            &reviewed_commit,
            "epic/test-feature",
            50
        )
        .unwrap(),
        "original reviewed commit should be patch-equivalent to rebased commit on epic branch"
    );

    // Step 4: Second integrate → complete via patch-equivalence.
    let second_integrate = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert!(
        second_integrate.is_error.is_none(),
        "{}",
        extract_text(&second_integrate)
    );
    let second_result: Value = serde_json::from_str(&extract_text(&second_integrate)).unwrap();
    assert_eq!(second_result["action"], "integrated");
    assert_eq!(second_result["integration_phase"], "complete");
    assert_eq!(second_result["status"], "integrated");
    assert_eq!(second_result["terminal_status"], "closed");
    assert_eq!(second_result["conflicting_files"], serde_json::json!([]));

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
}
