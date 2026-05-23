// Same env-serialization pattern as the lib tests — see lib.rs.
#![allow(clippy::await_holding_lock)]
// Tests construct config structs by mutating fields after `default()` to keep
// diffs tight when new fields land.
#![allow(clippy::field_reassign_with_default)]

//! Integration test for the full review flow.
//!
//! Tests the end-to-end review pipeline in an isolated temp directory:
//! 1. Create a task
//! 2. Request review → panel selected, review_id returned
//! 3. Submit reviews from multiple reviewers
//! 4. Verify consolidated report and outcome
//!
//! Uses direct VerificationTool calls with env var manipulation for
//! reviewer identity. All state is file-based under a temp BREHON_ROOT.
//!
//! **Must run with `--test-threads=1`** due to shared env var mutation.
//! `cargo test -p brehon-mcp --test review_flow -- --test-threads=1`

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Mutex;
use std::{path::Path, process::Command};

use brehon_mcp::server::ContentBlock;
use brehon_mcp::tools::task_actions::TaskActionsTool;
use brehon_mcp::tools::Tool;
use serde_json::json;

/// Global lock to serialize tests that mutate env vars.
struct EnvLock(Mutex<()>);

impl EnvLock {
    const fn new() -> Self {
        Self(Mutex::new(()))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>, std::convert::Infallible> {
        Ok(self.0.lock().unwrap_or_else(|poison| poison.into_inner()))
    }
}

static ENV_LOCK: EnvLock = EnvLock::new();
use brehon_mcp::tools::verification::{ReviewMaintenanceAction, VerificationTool};
use brehon_types::config::{ReviewConfig, ReviewLeaseMode, ReviewPanelConfig, ReviewPanelMode};
use brehon_types::review::ReviewPolicy;

/// Extract the text content from a ToolResult.
fn extract_text(result: &brehon_mcp::server::ToolResult) -> String {
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

/// Parse the result text as JSON.
fn parse_result(result: &brehon_mcp::server::ToolResult) -> serde_json::Value {
    let text = extract_text(result);
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse result as JSON: {e}\nText: {text}"))
}

struct ScopedBrehonEnv {
    saved: Vec<(OsString, Option<OsString>)>,
}

impl ScopedBrehonEnv {
    fn set(vars: &[(&'static str, &OsStr)]) -> Self {
        let mut saved = Vec::new();
        let mut tracked_keys = Vec::new();

        for (key, value) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BREHON_") {
                tracked_keys.push(key.clone());
                saved.push((key, Some(value)));
            }
        }

        for (key, _) in vars {
            let key = OsString::from(key);
            if !tracked_keys.iter().any(|existing| existing == &key) {
                tracked_keys.push(key.clone());
                saved.push((key, None));
            }
        }

        for key in &tracked_keys {
            std::env::remove_var(key);
        }
        for (key, value) in vars {
            std::env::set_var(key, value);
        }

        Self { saved }
    }
}

impl Drop for ScopedBrehonEnv {
    fn drop(&mut self) {
        let current_keys: Vec<OsString> = std::env::vars_os()
            .filter_map(|(key, _)| key.to_string_lossy().starts_with("BREHON_").then_some(key))
            .collect();
        for key in current_keys {
            std::env::remove_var(key);
        }
        for (key, value) in &self.saved {
            if let Some(value) = value {
                std::env::set_var(key, value);
            }
        }
    }
}

/// Set up a temp directory as BREHON_ROOT with a task file and reviewer sessions.
struct TestEnv {
    _env: ScopedBrehonEnv,
    workspace: PathBuf,
    root: PathBuf,
    task_id: String,
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
    // Mirror real worker worktrees: workers must operate on a dedicated
    // branch, not on the project default. Aligns with the
    // ensure_worker_branch_safe_for_task / ensure_checkpoint_cwd_is_isolated
    // guards in brehon-mcp::tools::task_actions::git_ops.
    run_git(workspace, &["checkout", "-b", "worker/test"]);
    run_git(workspace, &["rev-parse", "HEAD"])
}

impl TestEnv {
    fn new() -> Self {
        let workspace = std::env::temp_dir()
            .join("brehon-review-test")
            .join(format!("t-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        init_git_workspace(&workspace);
        let root = workspace.join(".brehon");
        std::fs::create_dir_all(&root).unwrap();

        // Clear inherited BREHON_* vars, then set only the values this test
        // environment needs.
        let env = ScopedBrehonEnv::set(&[
            ("BREHON_ROOT", root.as_os_str()),
            ("BREHON_WORKSPACE_ROOT", workspace.as_os_str()),
            ("BREHON_PROJECT_ROOT", OsStr::new("")),
            ("BREHON_WORKTREE_BRANCH", OsStr::new("")),
        ]);

        let task_id = format!(
            "T-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("test")
        );

        // Create task file
        let tasks_dir = root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task = serde_json::json!({
            "id": task_id,
            "task_id": task_id,
            "title": "Test task",
            "description": "A test task for review flow",
            "status": "in_progress",
            "task_type": "task",
            "completion_mode": "merge",
            "assignee": "worker-1"
        });
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();

        // Create reviewer session files so select_panel() finds them
        let sessions_dir = root.join("runtime").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        for (name, agent_type) in [("reviewer-alpha", "codex"), ("reviewer-beta", "gemini")] {
            let session = serde_json::json!({
                "name": name,
                "role": "reviewer",
                "agent_type": agent_type,
                "started_at": chrono::Utc::now().to_rfc3339()
            });
            std::fs::write(
                sessions_dir.join(format!("{name}.json")),
                serde_json::to_string_pretty(&session).unwrap(),
            )
            .unwrap();
        }

        // Also create a supervisor session for notifications
        let sup_session = serde_json::json!({
            "name": "supervisor-1",
            "role": "supervisor",
            "started_at": chrono::Utc::now().to_rfc3339()
        });
        std::fs::write(
            sessions_dir.join("supervisor-1.json"),
            serde_json::to_string_pretty(&sup_session).unwrap(),
        )
        .unwrap();

        Self {
            _env: env,
            workspace,
            root,
            task_id,
        }
    }

    fn create_task_with_details(
        &self,
        title: &str,
        description: &str,
        assignee: &str,
        completion_mode: &str,
    ) -> String {
        let task_id = format!(
            "T-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("test")
        );

        let tasks_dir = self.root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task = serde_json::json!({
            "id": task_id,
            "task_id": task_id,
            "title": title,
            "description": description,
            "status": "in_progress",
            "task_type": "task",
            "completion_mode": completion_mode,
            "assignee": assignee
        });
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();

        task_id
    }

    fn write_session(&self, name: &str, role: &str, agent_type: Option<&str>) {
        let sessions_dir = self.root.join("runtime").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let mut session = serde_json::json!({
            "name": name,
            "role": role,
            "started_at": chrono::Utc::now().to_rfc3339(),
            "last_seen_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(agent_type) = agent_type {
            session["agent_type"] = serde_json::json!(agent_type);
        }

        std::fs::write(
            sessions_dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(&session).unwrap(),
        )
        .unwrap();
    }

    fn remove_session(&self, name: &str) {
        let sessions_dir = self.root.join("runtime").join("sessions");
        let path = sessions_dir.join(format!("{name}.json"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }

    fn write_panel_seat(&self, panel_id: &str, members: &[(&str, &str)]) {
        let seats_dir = self.root.join("runtime").join("review-panel-seats");
        std::fs::create_dir_all(&seats_dir).unwrap();
        std::fs::write(
            seats_dir.join(format!("{panel_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "panel_id": panel_id,
                "members": members
                    .iter()
                    .map(|(slot_agent, reviewer)| serde_json::json!({
                        "slot_agent": slot_agent,
                        "reviewer": reviewer,
                    }))
                    .collect::<Vec<_>>(),
                "updated_at": chrono::Utc::now().to_rfc3339(),
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn make_tool(&self) -> VerificationTool {
        let policy = ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        };
        let config = ReviewConfig {
            policy,
            timeout_minutes: 30,
            auto_assign: true,
            default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
            panel_mode: ReviewPanelMode::FullCouncil,
            ..ReviewConfig::default()
        };
        VerificationTool::new().with_config(config)
    }
}

fn write_reviewer_reset_ack(root: &Path, task_id: &str, review_id: &str, reviewer: &str) {
    let dir = root.join("runtime").join("reviewer-reset-acks");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{task_id}--{review_id}--{reviewer}.json"));
    std::fs::write(
        path,
        serde_json::json!({
            "task_id": task_id,
            "review_id": review_id,
            "reviewer": reviewer,
            "reset_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

fn queued_messages_for(root: &std::path::Path, target: &str) -> Vec<String> {
    let queue_dir = root.join("runtime").join("prompt-queue");
    let mut messages = Vec::new();

    fn walk(dir: &std::path::Path, target: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                walk(&path, target, out);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.') || name.ends_with(".tmp"))
            {
                continue;
            }

            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
                continue;
            };
            let payload = parsed.get("entry").unwrap_or(&parsed);
            if payload.get("target").and_then(|value| value.as_str()) != Some(target) {
                continue;
            }
            if let Some(message) = payload.get("message").and_then(|value| value.as_str()) {
                out.push(message.to_string());
            }
        }
    }

    walk(&queue_dir, target, &mut messages);

    messages
}

#[tokio::test]
async fn test_full_review_approved_flow() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    // Set supervisor identity for request
    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    // Step 1: Request review
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Test task",
            "description": "Test description",
            "commit": "abc1234"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "request_review failed: {}",
        extract_text(&result)
    );
    let json = parse_result(&result);
    let review_id = json["review_id"].as_str().unwrap().to_string();
    assert!(review_id.starts_with("REV-"));
    assert_eq!(json["round"], 1);
    let panel = json["panel"].as_array().unwrap();
    assert_eq!(panel.len(), 2);

    // Step 2: First reviewer submits
    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");

    let result = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "score": 8,
            "verdict": "approved",
            "summary": "Looks good",
            "findings": []
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let json = parse_result(&result);
    assert_eq!(json["panel_progress"], "1/2");

    // Step 3: Second reviewer submits — triggers evaluation
    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");

    let result = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "score": 9,
            "verdict": "approved",
            "summary": "Excellent work"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let json = parse_result(&result);
    assert_eq!(json["outcome"], "approved");
    assert!(json["average_score"].as_f64().unwrap() >= 8.0);

    // Step 4: Verify review status
    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    let result = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();

    let json = parse_result(&result);
    assert_eq!(json["review_status"], "approved");

    // Step 5: Verify consolidated report was written
    let consolidated_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("round-1")
        .join("consolidated.json");
    assert!(consolidated_path.exists());
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&consolidated_path).unwrap()).unwrap();
    assert_eq!(report["outcome"], "approved");
}

#[tokio::test]
async fn test_request_review_prompt_explicitly_marks_paths_repo_relative() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Review path semantics",
            "description": "Verify reviewer prompt checkout guidance",
            "commit": "abc1234"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "request_review failed: {}",
        extract_text(&result)
    );

    let reviewer_messages = queued_messages_for(&env.root, "reviewer-alpha");
    assert!(
        reviewer_messages.iter().any(|message| {
            message.contains(
                "Paths: treat all file paths as repository-relative to your current worktree root.",
            ) && message.contains("Do not reinterpret them as another agent's checkout")
        }),
        "expected reviewer prompt to explain repo-relative path semantics, got: {reviewer_messages:?}"
    );
}

#[tokio::test]
async fn test_approved_audit_task_tells_supervisor_close_will_not_merge() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();
    let audit_task_id = env.create_task_with_details(
        "Audit review lifecycle",
        "Audit-only task with no code changes",
        "worker-1",
        "close",
    );

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": audit_task_id.clone(),
            "title": "Audit review lifecycle",
            "description": "Audit-only task with no code changes",
            "commit": "abc1234"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let first_submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 8,
            "verdict": "approved",
            "summary": "audit looks correct"
        }))
        .await
        .unwrap();
    assert!(
        first_submit.is_error.is_none(),
        "{}",
        extract_text(&first_submit)
    );

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    let second_submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 9,
            "verdict": "approved",
            "summary": "audit looks complete"
        }))
        .await
        .unwrap();
    assert!(
        second_submit.is_error.is_none(),
        "{}",
        extract_text(&second_submit)
    );

    let supervisor_messages = queued_messages_for(&env.root, "supervisor-1");
    assert!(
        supervisor_messages.iter().any(|message| {
            message.contains(&format!("Review complete for task {audit_task_id}"))
                && message.contains("Completion mode: close")
                && message.contains(&format!("task action=close id={audit_task_id}"))
                && message.contains("Task approved (awaiting close).")
                && message.contains("This will mark it as 'closed' without a merge.")
        }),
        "expected supervisor close-mode guidance in messages: {supervisor_messages:#?}"
    );
}

#[tokio::test]
async fn test_approved_merge_task_tells_supervisor_not_yet_merged() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Implement feature",
            "description": "Code changes in src/lib.rs",
            "commit": "abc1234"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    for (reviewer, score) in [("reviewer-alpha", 8), ("reviewer-beta", 9)] {
        std::env::set_var("BREHON_AGENT_NAME", reviewer);
        std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
        let submit = tool
            .execute(serde_json::json!({
                "action": "submit_review",
                "review_id": review_id,
                "reviewer": reviewer,
                "score": score,
                "verdict": "approved",
                "summary": "looks good"
            }))
            .await
            .unwrap();
        assert!(submit.is_error.is_none(), "{}", extract_text(&submit));
    }

    let supervisor_messages = queued_messages_for(&env.root, "supervisor-1");
    assert!(
        supervisor_messages.iter().any(|message| {
            message.contains(&format!("Review complete for task {}", env.task_id))
                && message.contains("Completion mode: merge")
                && message.contains("Task approved (not yet merged).")
                && message.contains("reviewed commit is on main")
        }),
        "expected supervisor merge-mode guidance in messages: {supervisor_messages:#?}"
    );
}

#[tokio::test]
async fn test_approved_epic_subtask_tells_supervisor_to_integrate_into_epic_branch() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task["parent_id"] = json!("E-feature");
    task["merge_target"] = json!("epic/feature-gate");
    task["integration_status"] = json!("pending");
    std::fs::write(&task_path, serde_json::to_string_pretty(&task).unwrap()).unwrap();

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Integrate feature subtask",
            "description": "Code changes for epic branch flow",
            "commit": "abc1234"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    for (reviewer, score) in [("reviewer-alpha", 8), ("reviewer-beta", 9)] {
        std::env::set_var("BREHON_AGENT_NAME", reviewer);
        std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
        let submit = tool
            .execute(serde_json::json!({
                "action": "submit_review",
                "review_id": review_id,
                "reviewer": reviewer,
                "score": score,
                "verdict": "approved",
                "summary": "looks good"
            }))
            .await
            .unwrap();
        assert!(submit.is_error.is_none(), "{}", extract_text(&submit));
    }

    let supervisor_messages = queued_messages_for(&env.root, "supervisor-1");
    assert!(
        supervisor_messages.iter().any(|message| {
            message.contains(&format!("Review complete for task {}", env.task_id))
                && message.contains("Task approved (awaiting merge-target integration).")
                && message.contains("Merge target: epic/feature-gate.")
                && message.contains("task action=integrate")
                && message.contains("Only a top-level container close may merge to main.")
                && !message.contains("task action=close")
        }),
        "expected epic-integration guidance in messages: {supervisor_messages:#?}"
    );
}

#[tokio::test]
async fn test_review_changes_requested_then_re_review() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    // Request review
    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Test",
        }))
        .await
        .unwrap();
    let review_id_1 = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Reviewer 1: approves with high score
    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    tool.execute(serde_json::json!({
        "action": "submit_review",
        "review_id": review_id_1,
        "score": 8,
        "verdict": "approved"
    }))
    .await
    .unwrap();

    // Reviewer 2: requests changes with blocking finding
    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    let result = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id_1,
            "score": 5,
            "verdict": "needs_revision",
            "findings": [
                {
                    "description": "Missing error handling",
                    "file": "src/main.rs",
                    "line": 42,
                    "severity": "blocking",
                    "suggestion": "Use ? operator"
                }
            ]
        }))
        .await
        .unwrap();

    let json = parse_result(&result);
    assert_eq!(json["outcome"], "changes_requested");

    // Now request re-review (round 2) — panel affinity preserved
    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Test (fixed)",
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "Re-review failed: {}",
        extract_text(&result)
    );
    let json = parse_result(&result);
    let review_id_2 = json["review_id"].as_str().unwrap().to_string();
    assert_ne!(review_id_1, review_id_2);
    assert_eq!(json["round"], 2);
    // Panel affinity: same reviewers
    let panel: Vec<String> = json["panel"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(panel.contains(&"reviewer-alpha".to_string()));
    assert!(panel.contains(&"reviewer-beta".to_string()));
}

#[tokio::test]
async fn test_review_round_closes_early_when_blocking_verdict_arrives() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.write_session("reviewer-gamma", "reviewer", Some("claude"));
    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec![
            "codex".to_string(),
            "gemini".to_string(),
            "claude".to_string(),
        ],
        panel_mode: ReviewPanelMode::FixedSize,
        panels: vec![ReviewPanelConfig {
            id: "primary".to_string(),
            reviewers: vec![
                "codex".to_string(),
                "gemini".to_string(),
                "claude".to_string(),
            ],
        }],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Three reviewer task"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let request_json = parse_result(&request);
    assert_eq!(request_json["panel"].as_array().unwrap().len(), 3);
    let review_id = request_json["review_id"].as_str().unwrap().to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let approval = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id,
            "score": 8,
            "verdict": "approved",
            "summary": "first pass ok"
        }))
        .await
        .unwrap();
    assert!(approval.is_error.is_none(), "{}", extract_text(&approval));
    let approval_json = parse_result(&approval);
    assert_eq!(approval_json["panel_progress"], "1/3");
    assert_eq!(approval_json["next_action"]["kind"], "wait_for_reviews");

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    let needs_revision = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id,
            "score": 5,
            "verdict": "needs_revision",
            "summary": "blocking issue",
            "findings": [{
                "description": "safe path still prompts",
                "file": "src/lib.rs",
                "line": 42,
                "severity": "blocking",
                "suggestion": "make unattended safe mode explicit"
            }]
        }))
        .await
        .unwrap();
    assert!(
        needs_revision.is_error.is_none(),
        "{}",
        extract_text(&needs_revision)
    );
    let json = parse_result(&needs_revision);
    assert_eq!(json["outcome"], "changes_requested");
    assert_eq!(json["completed_early"], true);
    assert_eq!(json["panel_progress"], "2/3");
    assert_eq!(json["next_action"]["kind"], "assign_revision_worker");

    let task: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            env.root
                .join("runtime")
                .join("tasks")
                .join(format!("{}.json", env.task_id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(task["status"], "changes_requested");

    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            env.root
                .join("runtime")
                .join("reviews")
                .join(&env.task_id)
                .join("state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["status"], "changes_requested");
    assert_eq!(state["submissions_received"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_rereview_refreshes_stale_panel_members_after_restart() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
        panel_mode: ReviewPanelMode::FixedSize,
        panels: vec![ReviewPanelConfig {
            id: "primary".to_string(),
            reviewers: vec!["codex".to_string(), "gemini".to_string()],
        }],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Round 1",
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let request_json = parse_result(&request);
    assert_eq!(request_json["panel_id"], "primary");
    let review_id_1 = request_json["review_id"].as_str().unwrap().to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let approve = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id_1,
            "score": 8,
            "verdict": "approved",
            "summary": "looks good"
        }))
        .await
        .unwrap();
    assert!(approve.is_error.is_none(), "{}", extract_text(&approve));

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let needs_revision = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id_1,
            "score": 5,
            "verdict": "needs_revision",
            "findings": [{
                "description": "needs a follow-up pass",
                "severity": "blocking"
            }]
        }))
        .await
        .unwrap();
    assert!(
        needs_revision.is_error.is_none(),
        "{}",
        extract_text(&needs_revision)
    );
    assert_eq!(
        parse_result(&needs_revision)["outcome"],
        "changes_requested"
    );

    env.remove_session("reviewer-alpha");
    env.write_session("reviewer-delta", "reviewer", Some("codex"));

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let rereview = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Round 2",
        }))
        .await
        .unwrap();
    assert!(rereview.is_error.is_none(), "{}", extract_text(&rereview));

    let rereview_json = parse_result(&rereview);
    assert_eq!(rereview_json["panel_id"], "primary");
    assert_eq!(rereview_json["round"], 2);
    let round_two_panel: Vec<String> = rereview_json["panel"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|value| value.as_str().map(String::from))
        .collect();
    assert!(round_two_panel.contains(&"reviewer-delta".to_string()));
    assert!(round_two_panel.contains(&"reviewer-beta".to_string()));
    assert!(!round_two_panel.contains(&"reviewer-alpha".to_string()));

    let lease: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            env.root
                .join("runtime")
                .join("review-panels")
                .join(format!("{}.json", env.task_id)),
        )
        .unwrap(),
    )
    .unwrap();
    let lease_members: Vec<String> = lease["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|member| member["reviewer"].as_str().map(String::from))
        .collect();
    assert!(lease_members.contains(&"reviewer-delta".to_string()));
    assert!(lease_members.contains(&"reviewer-beta".to_string()));
    assert!(!lease_members.contains(&"reviewer-alpha".to_string()));

    let stale_messages = queued_messages_for(&env.root, "reviewer-alpha");
    assert!(
        !stale_messages
            .iter()
            .any(|message| message.contains("Round 2")),
        "stale reviewer should not receive round-2 request: {stale_messages:#?}"
    );
    let new_messages = queued_messages_for(&env.root, "reviewer-delta");
    assert!(
        new_messages
            .iter()
            .any(|message| message.contains("Round 2")),
        "replacement reviewer should receive round-2 request: {new_messages:#?}"
    );
}

#[tokio::test]
async fn test_duplicate_submission_rejected() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
        }))
        .await
        .unwrap();
    let review_id = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    // First submission
    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let result = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "score": 8,
            "verdict": "approved"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none());

    // Duplicate submission — should fail
    let result = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "score": 9,
            "verdict": "approved"
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("already submitted"));
}

#[tokio::test]
async fn test_non_panel_reviewer_rejected() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
        }))
        .await
        .unwrap();
    let review_id = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Try to submit as a reviewer NOT on the panel
    std::env::set_var("BREHON_AGENT_NAME", "reviewer-gamma");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let result = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "score": 8,
            "verdict": "approved"
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("not on the panel"));
}

#[tokio::test]
async fn test_request_review_includes_all_eligible_council_reviewers() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.write_session("reviewer-gamma", "reviewer", Some("codex"));
    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
        panel_mode: ReviewPanelMode::FullCouncil,
        panels: vec![ReviewPanelConfig {
            id: "primary".to_string(),
            reviewers: vec![
                "codex".to_string(),
                "gemini".to_string(),
                "codex".to_string(),
            ],
        }],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json = parse_result(&result);
    let panel: Vec<String> = json["panel"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|reviewer| reviewer.as_str().map(String::from))
        .collect();
    assert_eq!(json["panel_id"], "primary");
    assert_eq!(panel.len(), 3);
    assert!(panel.contains(&"reviewer-alpha".to_string()));
    assert!(panel.contains(&"reviewer-beta".to_string()));
    assert!(panel.contains(&"reviewer-gamma".to_string()));
}

#[tokio::test]
async fn test_share_after_submit_keeps_reviewer_reserved_until_reset_ack() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.write_session("reviewer-gamma", "reviewer", Some("gemini"));

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
        panel_mode: ReviewPanelMode::FixedSize,
        lease_mode: ReviewLeaseMode::ShareAfterSubmit,
        panels: vec![ReviewPanelConfig {
            id: "primary".to_string(),
            reviewers: vec!["codex".to_string(), "gemini".to_string()],
        }],
        ..ReviewConfig::default()
    });

    let second_task = env.create_task_with_details(
        "Second review task",
        "Needs same codex slot",
        "worker-2",
        "merge",
    );
    env.write_session("worker-2", "worker", Some("codex"));

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let first = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Task one",
            "description": "Round one",
            "commit": "abc111"
        }))
        .await
        .unwrap();
    let first_json = parse_result(&first);
    assert_eq!(
        first_json["panel"],
        json!(["reviewer-alpha", "reviewer-beta"])
    );
    let first_review_id = first_json["review_id"].as_str().unwrap().to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let submit = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": first_review_id,
            "reviewer": "reviewer-alpha",
            "score": 8,
            "verdict": "approved",
            "summary": "Looks good",
            "findings": []
        }))
        .await
        .unwrap();
    let submit_json = parse_result(&submit);
    assert_eq!(submit_json["reviewer_reset_queued"], true);

    let reset_queue = env.root.join("runtime").join("reviewer-reset-queue");
    let queued = std::fs::read_dir(&reset_queue).unwrap().flatten().count();
    assert_eq!(queued, 1);

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let blocked = tool
        .execute(json!({
            "action": "request_review",
            "task_id": second_task,
            "title": "Task two",
            "description": "Blocked until reset",
            "commit": "abc222"
        }))
        .await
        .unwrap();
    assert_eq!(blocked.is_error, Some(true));
    assert!(
        extract_text(&blocked).contains("No idle review panels are currently available"),
        "{}",
        extract_text(&blocked)
    );

    write_reviewer_reset_ack(
        &env.root,
        &env.task_id,
        first_json["review_id"].as_str().unwrap(),
        "reviewer-alpha",
    );

    let second = tool
        .execute(json!({
            "action": "request_review",
            "task_id": second_task,
            "title": "Task two",
            "description": "Can start after reset",
            "commit": "abc222"
        }))
        .await
        .unwrap();
    let second_json = parse_result(&second);
    assert_eq!(
        second_json["panel"],
        json!(["reviewer-alpha", "reviewer-gamma"])
    );
}

#[tokio::test]
async fn test_share_after_submit_spreads_requests_across_configured_panels() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.write_session("reviewer-delta", "reviewer", Some("codex"));
    env.write_session("reviewer-epsilon", "reviewer", Some("gemini"));

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
        panel_mode: ReviewPanelMode::FixedSize,
        lease_mode: ReviewLeaseMode::ShareAfterSubmit,
        panels: vec![
            ReviewPanelConfig {
                id: "primary".to_string(),
                reviewers: vec!["codex".to_string(), "gemini".to_string()],
            },
            ReviewPanelConfig {
                id: "secondary".to_string(),
                reviewers: vec!["codex".to_string(), "gemini".to_string()],
            },
        ],
        ..ReviewConfig::default()
    });

    let second_task = env.create_task_with_details(
        "Second review task",
        "Should land on the second panel",
        "worker-2",
        "merge",
    );
    env.write_session("worker-2", "worker", Some("codex"));

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let first = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Task one"
        }))
        .await
        .unwrap();
    let first_json = parse_result(&first);
    assert_eq!(first_json["panel_id"], "primary");
    assert_eq!(
        first_json["panel"],
        json!(["reviewer-alpha", "reviewer-beta"])
    );

    let second = tool
        .execute(json!({
            "action": "request_review",
            "task_id": second_task,
            "title": "Task two"
        }))
        .await
        .unwrap();
    let second_json = parse_result(&second);
    assert_eq!(second_json["panel_id"], "secondary");
    assert_eq!(
        second_json["panel"],
        json!(["reviewer-delta", "reviewer-epsilon"])
    );
}

#[tokio::test]
async fn test_configured_panel_uses_persisted_physical_seats() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.remove_session("reviewer-alpha");
    env.remove_session("reviewer-beta");
    for (name, agent_type) in [
        ("a-claude", "claude-reviewer"),
        ("z-claude", "claude-reviewer"),
        ("a-codex", "codex-reviewer"),
        ("z-codex", "codex-reviewer"),
        ("a-gemini", "gemini-reviewer"),
        ("z-gemini", "gemini-reviewer"),
    ] {
        env.write_session(name, "reviewer", Some(agent_type));
    }
    env.write_panel_seat(
        "primary",
        &[
            ("claude-reviewer", "z-claude"),
            ("codex-reviewer", "z-codex"),
            ("gemini-reviewer", "z-gemini"),
        ],
    );
    env.write_panel_seat(
        "secondary",
        &[
            ("claude-reviewer", "a-claude"),
            ("codex-reviewer", "a-codex"),
            ("gemini-reviewer", "a-gemini"),
        ],
    );

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec![
            "claude-reviewer".to_string(),
            "codex-reviewer".to_string(),
            "gemini-reviewer".to_string(),
        ],
        panel_mode: ReviewPanelMode::FullCouncil,
        panels: vec![
            ReviewPanelConfig {
                id: "primary".to_string(),
                reviewers: vec![
                    "claude-reviewer".to_string(),
                    "codex-reviewer".to_string(),
                    "gemini-reviewer".to_string(),
                ],
            },
            ReviewPanelConfig {
                id: "secondary".to_string(),
                reviewers: vec![
                    "claude-reviewer".to_string(),
                    "codex-reviewer".to_string(),
                    "gemini-reviewer".to_string(),
                ],
            },
        ],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Physical primary panel"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let result_json = parse_result(&result);
    assert_eq!(result_json["panel_id"], "primary");
    assert_eq!(
        result_json["panel"],
        json!(["z-claude", "z-codex", "z-gemini"])
    );
}

#[tokio::test]
async fn test_share_after_submit_keeps_configured_panel_atomic_until_round_complete() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.remove_session("reviewer-alpha");
    env.remove_session("reviewer-beta");
    for (name, agent_type) in [
        ("primary-codex", "codex"),
        ("primary-gemini", "gemini"),
        ("secondary-codex", "codex"),
        ("secondary-gemini", "gemini"),
    ] {
        env.write_session(name, "reviewer", Some(agent_type));
    }
    env.write_panel_seat(
        "primary",
        &[("codex", "primary-codex"), ("gemini", "primary-gemini")],
    );
    env.write_panel_seat(
        "secondary",
        &[("codex", "secondary-codex"), ("gemini", "secondary-gemini")],
    );

    let second_task = env.create_task_with_details(
        "Second review task",
        "Must not partially reuse primary while primary-gemini is still reviewing",
        "worker-2",
        "merge",
    );
    env.write_session("worker-2", "worker", Some("codex"));

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
        panel_mode: ReviewPanelMode::FixedSize,
        lease_mode: ReviewLeaseMode::ShareAfterSubmit,
        panels: vec![
            ReviewPanelConfig {
                id: "primary".to_string(),
                reviewers: vec!["codex".to_string(), "gemini".to_string()],
            },
            ReviewPanelConfig {
                id: "secondary".to_string(),
                reviewers: vec!["codex".to_string(), "gemini".to_string()],
            },
        ],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let first = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Task one"
        }))
        .await
        .unwrap();
    let first_json = parse_result(&first);
    assert_eq!(first_json["panel_id"], "primary");
    assert_eq!(
        first_json["panel"],
        json!(["primary-codex", "primary-gemini"])
    );
    let first_review_id = first_json["review_id"].as_str().unwrap().to_string();

    std::env::set_var("BREHON_AGENT_NAME", "primary-codex");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let partial = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": first_review_id,
            "score": 8,
            "verdict": "approved",
            "summary": "first primary reviewer is done"
        }))
        .await
        .unwrap();
    assert!(partial.is_error.is_none(), "{}", extract_text(&partial));
    write_reviewer_reset_ack(&env.root, &env.task_id, &first_review_id, "primary-codex");

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let second = tool
        .execute(json!({
            "action": "request_review",
            "task_id": second_task,
            "title": "Task two"
        }))
        .await
        .unwrap();
    assert!(second.is_error.is_none(), "{}", extract_text(&second));
    let second_json = parse_result(&second);
    assert_eq!(second_json["panel_id"], "secondary");
    assert_eq!(
        second_json["panel"],
        json!(["secondary-codex", "secondary-gemini"])
    );
}

#[tokio::test]
async fn test_share_after_submit_defers_final_reviewer_reset_until_round_close_succeeds() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 1,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string()],
        panel_mode: ReviewPanelMode::FixedSize,
        lease_mode: ReviewLeaseMode::ShareAfterSubmit,
        panels: vec![ReviewPanelConfig {
            id: "solo".to_string(),
            reviewers: vec!["codex".to_string()],
        }],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Single reviewer task",
            "description": "Should not queue reset if closeout fails",
            "commit": "abc123"
        }))
        .await
        .unwrap();
    let request_json = parse_result(&request);
    let review_id = request_json["review_id"].as_str().unwrap().to_string();

    let consolidated_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("round-1")
        .join("consolidated.json");
    std::fs::create_dir_all(&consolidated_path).unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let submit = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 9,
            "verdict": "approved",
            "summary": "Looks good",
            "findings": []
        }))
        .await
        .unwrap();

    assert!(
        extract_text(&submit).contains("Failed to persist consolidated report"),
        "{}",
        extract_text(&submit)
    );

    let reset_queue = env.root.join("runtime").join("reviewer-reset-queue");
    let queued = if reset_queue.exists() {
        std::fs::read_dir(&reset_queue).unwrap().flatten().count()
    } else {
        0
    };
    assert_eq!(queued, 0);
}

#[tokio::test]
async fn test_share_after_submit_queues_final_reviewer_reset_after_successful_round_close() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 1,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string()],
        panel_mode: ReviewPanelMode::FixedSize,
        lease_mode: ReviewLeaseMode::ShareAfterSubmit,
        panels: vec![ReviewPanelConfig {
            id: "solo".to_string(),
            reviewers: vec!["codex".to_string()],
        }],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Single reviewer success",
            "description": "Should queue reset after closeout succeeds",
            "commit": "abc123"
        }))
        .await
        .unwrap();
    let request_json = parse_result(&request);
    let review_id = request_json["review_id"].as_str().unwrap().to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let submit = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 9,
            "verdict": "approved",
            "summary": "Looks good",
            "findings": []
        }))
        .await
        .unwrap();
    let submit_json = parse_result(&submit);
    assert_eq!(submit_json["reviewer_reset_queued"], true);

    let reset_queue = env.root.join("runtime").join("reviewer-reset-queue");
    let queued = std::fs::read_dir(&reset_queue).unwrap().flatten().count();
    assert_eq!(queued, 1);
}

#[tokio::test]
async fn test_panel_lease_blocks_second_task_until_first_task_is_terminal() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();
    let task_tool = TaskActionsTool;
    let first_task = env.create_task_with_details(
        "Panel owner",
        "First task should own the only panel until terminal close",
        "worker-1",
        "close",
    );
    let second_task = env.create_task_with_details(
        "Queued task",
        "Second task must wait for the panel to be released",
        "worker-2",
        "close",
    );
    env.write_session("worker-2", "worker", Some("codex"));

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let first_request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": first_task,
            "title": "First leased review"
        }))
        .await
        .unwrap();
    assert!(
        first_request.is_error.is_none(),
        "{}",
        extract_text(&first_request)
    );
    let first_review_id = parse_result(&first_request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    tool.execute(json!({
        "action": "submit_review",
        "review_id": first_review_id,
        "reviewer": "reviewer-alpha",
        "score": 9,
        "verdict": "approved"
    }))
    .await
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    tool.execute(json!({
        "action": "submit_review",
        "review_id": parse_result(&first_request)["review_id"],
        "reviewer": "reviewer-beta",
        "score": 8,
        "verdict": "approved"
    }))
    .await
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let blocked = tool
        .execute(json!({
            "action": "request_review",
            "task_id": second_task,
            "title": "Second review should queue"
        }))
        .await
        .unwrap();
    assert_eq!(blocked.is_error, Some(true));
    let blocked_text = extract_text(&blocked);
    assert!(blocked_text.contains("No idle review panels"));
    assert!(blocked_text.contains("default-panel"));
    assert!(blocked_text.contains(&first_task));

    let close = task_tool
        .execute(json!({
            "action": "close",
            "id": first_task,
            "agent_name": "supervisor-1",
            "role": "supervisor",
            "supervisor": "supervisor-1"
        }))
        .await
        .unwrap();
    assert!(close.is_error.is_none(), "{}", extract_text(&close));
    let close_json = parse_result(&close);
    assert_eq!(close_json["released_panel"], "default-panel");

    let second_request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": second_task,
            "title": "Second review after release"
        }))
        .await
        .unwrap();
    assert!(
        second_request.is_error.is_none(),
        "{}",
        extract_text(&second_request)
    );
    let second_json = parse_result(&second_request);
    assert_eq!(second_json["panel_id"], "default-panel");
}

#[tokio::test]
async fn test_configured_panels_allow_parallel_review_lanes() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.write_session("reviewer-gamma", "reviewer", Some("codex"));
    env.write_session("reviewer-delta", "reviewer", Some("gemini"));

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
        panel_mode: ReviewPanelMode::FullCouncil,
        panels: vec![
            ReviewPanelConfig {
                id: "panel-a".to_string(),
                reviewers: vec!["codex".to_string(), "gemini".to_string()],
            },
            ReviewPanelConfig {
                id: "panel-b".to_string(),
                reviewers: vec!["codex".to_string(), "gemini".to_string()],
            },
        ],
        ..ReviewConfig::default()
    });

    let first_task = env.create_task_with_details(
        "Panel A task",
        "Should lease one panel",
        "worker-a",
        "close",
    );
    let second_task = env.create_task_with_details(
        "Panel B task",
        "Should lease the second panel",
        "worker-b",
        "close",
    );
    env.write_session("worker-b", "worker", Some("codex"));

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let first = tool
        .execute(json!({
            "action": "request_review",
            "task_id": first_task,
            "title": "First panel lane"
        }))
        .await
        .unwrap();
    assert!(first.is_error.is_none(), "{}", extract_text(&first));
    let first_json = parse_result(&first);
    let first_panel: Vec<String> = first_json["panel"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|reviewer| reviewer.as_str().map(String::from))
        .collect();

    let second = tool
        .execute(json!({
            "action": "request_review",
            "task_id": second_task,
            "title": "Second panel lane"
        }))
        .await
        .unwrap();
    assert!(second.is_error.is_none(), "{}", extract_text(&second));
    let second_json = parse_result(&second);
    let second_panel: Vec<String> = second_json["panel"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|reviewer| reviewer.as_str().map(String::from))
        .collect();

    assert_ne!(first_json["panel_id"], second_json["panel_id"]);
    assert_eq!(first_panel.len(), 2);
    assert_eq!(second_panel.len(), 2);
    assert!(first_panel
        .iter()
        .all(|reviewer| !second_panel.contains(reviewer)));
}

#[tokio::test]
async fn test_request_review_reports_missing_panel_lane_instead_of_no_idle_panels() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.write_session("reviewer-alpha", "reviewer", Some("claude-reviewer"));
    env.write_session("reviewer-beta", "reviewer", Some("codex-reviewer"));
    env.write_session("reviewer-gamma", "reviewer", Some("gemini-reviewer"));

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec![
            "claude-reviewer".to_string(),
            "codex-reviewer".to_string(),
            "gemini-reviewer".to_string(),
        ],
        panel_mode: ReviewPanelMode::FullCouncil,
        panels: vec![ReviewPanelConfig {
            id: "primary".to_string(),
            reviewers: vec![
                "claude-reviewer".to_string(),
                "codex-reviewer".to_string(),
                "copilot-reviewer".to_string(),
            ],
        }],
        ..ReviewConfig::default()
    });

    let result = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Broken panel config"
        }))
        .await
        .unwrap();

    let text = extract_text(&result);
    assert!(text.contains("copilot-reviewer"), "{text}");
    assert!(text.contains("primary missing"), "{text}");
    assert!(text.contains("claude-reviewer x1"), "{text}");
    assert!(text.contains("codex-reviewer x1"), "{text}");
    assert!(text.contains("gemini-reviewer x1"), "{text}");
    assert!(!text.contains("No idle review panels"), "{text}");
}

#[tokio::test]
async fn test_approved_override_is_rejected() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    // Request review
    tool.execute(serde_json::json!({
        "action": "request_review",
        "task_id": env.task_id,
    }))
    .await
    .unwrap();

    // Approval override is a review bypass and must be rejected.
    let result = tool
        .execute(serde_json::json!({
            "action": "override",
            "task_id": env.task_id,
            "verdict": "approved",
            "reason": "Low risk change, expediting"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("approval override is disabled"));

    // Verify status remains in the active review round.
    let result = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let json = parse_result(&result);
    assert_eq!(json["review_status"], "collecting");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "in_review");
}

#[tokio::test]
async fn test_reset_rounds_allows_fresh_review_cycle_after_max_rounds() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    for expected_round in 1..=3 {
        let request = tool
            .execute(serde_json::json!({
                "action": "request_review",
                "task_id": env.task_id,
                "title": format!("Round {expected_round}")
            }))
            .await
            .unwrap();
        assert!(request.is_error.is_none(), "{}", extract_text(&request));
        let request_json = parse_result(&request);
        let review_id = request_json["review_id"].as_str().unwrap().to_string();
        assert_eq!(request_json["round"], expected_round);

        for (reviewer, score, verdict) in [
            ("reviewer-alpha", 8, "approved"),
            ("reviewer-beta", 5, "needs_revision"),
        ] {
            std::env::set_var("BREHON_AGENT_NAME", reviewer);
            std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
            let submit = tool
                .execute(serde_json::json!({
                    "action": "submit_review",
                    "review_id": review_id,
                    "reviewer": reviewer,
                    "score": score,
                    "verdict": verdict,
                    "summary": "needs another pass"
                }))
                .await
                .unwrap();
            assert!(submit.is_error.is_none(), "{}", extract_text(&submit));
        }

        std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
        std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    }

    let status = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_json = parse_result(&status);
    assert_eq!(status_json["review_status"], "escalated");
    assert_eq!(status_json["cycle_round"], 3);
    assert_eq!(
        status_json["action_needed"],
        "reset_rounds_or_negative_override"
    );

    let blocked_request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    assert_eq!(blocked_request.is_error, Some(true));
    assert!(extract_text(&blocked_request).contains("reset_rounds"));

    let reset = tool
        .execute(serde_json::json!({
            "action": "reset_rounds",
            "task_id": env.task_id,
            "reason": "Round 4 fixes warrant another fresh panel pass"
        }))
        .await
        .unwrap();
    assert!(reset.is_error.is_none(), "{}", extract_text(&reset));
    let reset_json = parse_result(&reset);
    assert_eq!(reset_json["next_round"], 4);
    assert_eq!(reset_json["next_cycle_round"], 1);

    let rerequest = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Round 4 after reset"
        }))
        .await
        .unwrap();
    assert!(rerequest.is_error.is_none(), "{}", extract_text(&rerequest));
    let rerequest_json = parse_result(&rerequest);
    assert_eq!(rerequest_json["round"], 4);
    assert_eq!(rerequest_json["cycle_round"], 1);
}

#[tokio::test]
async fn test_request_review_rejects_terminal_task_status() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task["status"] = serde_json::json!("merged");
    std::fs::write(&task_path, serde_json::to_string_pretty(&task).unwrap()).unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("cannot enter review"));
}

#[tokio::test]
async fn test_override_rejects_terminal_task_status() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task["status"] = serde_json::json!("merged");
    std::fs::write(&task_path, serde_json::to_string_pretty(&task).unwrap()).unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "override",
            "task_id": env.task_id,
            "verdict": "approved",
            "reason": "late override should fail"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("already terminal"));
}

#[tokio::test]
async fn test_late_submission_after_override_is_ignored_without_tool_error() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Late override handling",
        }))
        .await
        .unwrap();
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    let override_result = tool
        .execute(serde_json::json!({
            "action": "override",
            "task_id": env.task_id,
            "verdict": "rejected",
            "reason": "Supervisor rejected stale review round"
        }))
        .await
        .unwrap();
    assert!(
        override_result.is_error.is_none(),
        "{}",
        extract_text(&override_result)
    );

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let late_submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 8,
            "verdict": "approved",
            "summary": "late review should be ignored"
        }))
        .await
        .unwrap();

    assert!(
        late_submit.is_error.is_none(),
        "late submit should be ignored without tool failure: {}",
        extract_text(&late_submit)
    );
    let late_json = parse_result(&late_submit);
    assert_eq!(late_json["ignored"], true);
    // Production unified the late-submit rejection reasons; the override
    // path drops the review state, so a follow-up submit is "round_superseded"
    // (see `RejectionReason::RoundSuperseded` in tools/verification/tool.rs).
    assert_eq!(late_json["reason"], "round_superseded");
    assert_eq!(late_json["review_status"], "rejected");
    assert_eq!(late_json["task_status"], "changes_requested");
}

#[tokio::test]
async fn test_override_rejected_sets_task_back_to_changes_requested() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Rejected override handling",
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));

    let override_result = tool
        .execute(serde_json::json!({
            "action": "override",
            "task_id": env.task_id,
            "verdict": "rejected",
            "reason": "Needs rework"
        }))
        .await
        .unwrap();
    assert!(
        override_result.is_error.is_none(),
        "{}",
        extract_text(&override_result)
    );

    let status = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_json = parse_result(&status);
    assert_eq!(status_json["review_status"], "rejected");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "changes_requested");
}

#[tokio::test]
async fn test_task_mine_for_reviewer_includes_active_review_obligation() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();
    let task_tool = TaskActionsTool;

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Review obligation visibility",
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let mine = task_tool
        .execute(serde_json::json!({
            "action": "mine"
        }))
        .await
        .unwrap();
    assert!(mine.is_error.is_none(), "{}", extract_text(&mine));
    let mine_json = parse_result(&mine);
    assert_eq!(mine_json["tasks"].as_array().unwrap().len(), 0);
    assert_eq!(mine_json["review_count"], 1);
    let obligations = mine_json["review_obligations"].as_array().unwrap();
    assert_eq!(obligations.len(), 1);
    assert_eq!(obligations[0]["task_id"], env.task_id);
    assert_eq!(obligations[0]["review_id"], review_id);
    assert_eq!(obligations[0]["status"], "collecting");
    assert_eq!(obligations[0]["panel_id"], "default-panel");
    assert_eq!(obligations[0]["panel_lease_state"], "leased");
}

#[tokio::test]
async fn test_task_mine_for_reviewer_clears_obligation_after_submit_and_override() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();
    let task_tool = TaskActionsTool;

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Review obligation clearing",
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 8,
            "verdict": "approved",
            "summary": "done"
        }))
        .await
        .unwrap();
    assert!(submit.is_error.is_none(), "{}", extract_text(&submit));

    let mine_after_submit = task_tool
        .execute(serde_json::json!({
            "action": "mine"
        }))
        .await
        .unwrap();
    let mine_after_submit_json = parse_result(&mine_after_submit);
    assert_eq!(mine_after_submit_json["review_count"], 0);
    assert!(mine_after_submit_json["review_obligations"]
        .as_array()
        .unwrap()
        .is_empty());

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let mine_pending = task_tool
        .execute(serde_json::json!({
            "action": "mine"
        }))
        .await
        .unwrap();
    let mine_pending_json = parse_result(&mine_pending);
    assert_eq!(mine_pending_json["review_count"], 1);

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let override_result = tool
        .execute(serde_json::json!({
            "action": "override",
            "task_id": env.task_id,
            "verdict": "rejected",
            "reason": "Force reviewer obligation cancellation for test"
        }))
        .await
        .unwrap();
    assert!(
        override_result.is_error.is_none(),
        "{}",
        extract_text(&override_result)
    );

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let mine_after_override = task_tool
        .execute(serde_json::json!({
            "action": "mine"
        }))
        .await
        .unwrap();
    let mine_after_override_json = parse_result(&mine_after_override);
    assert_eq!(mine_after_override_json["review_count"], 0);
    assert!(mine_after_override_json["review_obligations"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_changes_requested_persists_review_feedback_and_notifies_worker() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();
    let task_tool = TaskActionsTool;
    env.write_session("worker-1", "worker", Some("opencode"));
    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut seeded_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    seeded_task["percent"] = serde_json::json!(100);
    seeded_task["activity"] = serde_json::json!("testing");
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&seeded_task).unwrap(),
    )
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Persist blocker feedback"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let alpha = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 4,
            "verdict": "needs_revision",
            "summary": "Blocking issue remains",
            "findings": [{
                "description": "Guard the failpoint behind test-only input",
                "file": "crates/brehon-tui/src/run.rs",
                "line": 412,
                "severity": "blocking",
                "suggestion": "Use a cfg(test) hook instead of runtime input"
            }]
        }))
        .await
        .unwrap();
    assert!(alpha.is_error.is_none(), "{}", extract_text(&alpha));
    let alpha_json = parse_result(&alpha);
    assert_eq!(alpha_json["outcome"], "changes_requested");
    assert_eq!(alpha_json["completed_early"], true);

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let beta = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 7,
            "verdict": "approved",
            "summary": "One follow-up suggestion",
            "findings": [{
                "description": "Add a stale-context regression for the helper",
                "severity": "suggestion"
            }]
        }))
        .await
        .unwrap();
    assert!(beta.is_error.is_none(), "{}", extract_text(&beta));

    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "changes_requested");
    assert_eq!(task["assignee"], "worker-1");
    assert_eq!(task["review_owner"], "worker-1");
    assert_eq!(task["percent"], 0);
    assert!(
        task.get("activity").is_none(),
        "stale activity should be cleared"
    );
    assert_eq!(task["review_feedback"]["review_id"], review_id);
    assert_eq!(task["review_feedback"]["outcome"], "changes_requested");
    assert!(!task["review_feedback"]["threshold_reason"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
    assert_eq!(
        task["review_feedback"]["blocking"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        task["review_feedback"]["suggestions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let worker_messages = queued_messages_for(&env.root, "worker-1");
    assert!(
        worker_messages.iter().any(|message| {
            message.contains(&format!("Review feedback for task {}", env.task_id))
                && message.contains("Guard the failpoint behind test-only input")
                && message.contains("task action=mine")
        }),
        "expected worker feedback message, got: {worker_messages:#?}"
    );

    std::env::set_var("BREHON_AGENT_NAME", "worker-1");
    std::env::set_var("BREHON_AGENT_ROLE", "worker");
    let mine = task_tool
        .execute(serde_json::json!({
            "action": "mine"
        }))
        .await
        .unwrap();
    let mine_json = parse_result(&mine);
    let tasks = mine_json["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["review_feedback"]["outcome"], "changes_requested");
}

#[tokio::test]
async fn test_changes_requested_keeps_dead_review_owner_unassigned() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();
    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut seeded_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    seeded_task["percent"] = serde_json::json!(100);
    seeded_task["activity"] = serde_json::json!("testing");
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&seeded_task).unwrap(),
    )
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Do not restore dead review owner"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let alpha = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 4,
            "verdict": "needs_revision",
            "summary": "Blocking issue remains",
            "findings": [{
                "description": "Guard the failpoint behind test-only input",
                "file": "crates/brehon-tui/src/run.rs",
                "line": 412,
                "severity": "blocking",
                "suggestion": "Use a cfg(test) hook instead of runtime input"
            }]
        }))
        .await
        .unwrap();
    assert!(alpha.is_error.is_none(), "{}", extract_text(&alpha));

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let beta = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 7,
            "verdict": "approved",
            "summary": "One follow-up suggestion",
            "findings": [{
                "description": "Add a stale-context regression for the helper",
                "severity": "suggestion"
            }]
        }))
        .await
        .unwrap();
    assert!(beta.is_error.is_none(), "{}", extract_text(&beta));

    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "changes_requested");
    assert_eq!(task["assignee"], serde_json::Value::Null);
    assert_eq!(task["review_owner"], "worker-1");
    assert_eq!(task["percent"], 0);
    assert!(
        task.get("activity").is_none(),
        "stale activity should be cleared"
    );

    let worker_messages = queued_messages_for(&env.root, "worker-1");
    assert!(
        worker_messages.is_empty(),
        "dead worker should not be notified: {worker_messages:#?}"
    );
}

#[tokio::test]
async fn test_approved_review_persists_followups_on_task() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Persist approved followups"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let alpha = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 8,
            "verdict": "approved",
            "summary": "Looks good; one cleanup later",
            "findings": [{
                "description": "Extract the retry backoff literal into a named constant",
                "file": "crates/brehon-mcp/src/tools/verification.rs",
                "line": 100,
                "severity": "suggestion",
                "suggestion": "Use a shared const for the retry delay"
            }]
        }))
        .await
        .unwrap();
    assert!(alpha.is_error.is_none(), "{}", extract_text(&alpha));

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let beta = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 9,
            "verdict": "approved",
            "summary": "Approved with one nitpick",
            "findings": [{
                "description": "Tighten the doc comment around reset_rounds",
                "severity": "nitpick"
            }]
        }))
        .await
        .unwrap();
    assert!(beta.is_error.is_none(), "{}", extract_text(&beta));

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "approved");
    assert_eq!(task["review_feedback"]["outcome"], "approved");
    let followups = task["review_followups"].as_array().unwrap();
    assert_eq!(followups.len(), 2);
    assert!(followups
        .iter()
        .all(|followup| followup["status"] == "open"));
    assert!(followups.iter().any(|followup| {
        followup["description"] == "Extract the retry backoff literal into a named constant"
    }));
    assert!(followups.iter().any(|followup| {
        followup["description"] == "Tighten the doc comment around reset_rounds"
    }));
}

#[tokio::test]
async fn test_request_review_clears_stale_task_review_feedback() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Clear stale feedback on rereview"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let alpha = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 4,
            "verdict": "needs_revision",
            "summary": "Still failing"
        }))
        .await
        .unwrap();
    assert!(alpha.is_error.is_none(), "{}", extract_text(&alpha));

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let beta = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 7,
            "verdict": "approved",
            "summary": "Looks otherwise okay"
        }))
        .await
        .unwrap();
    assert!(beta.is_error.is_none(), "{}", extract_text(&beta));

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task_before: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert!(task_before.get("review_feedback").is_some());

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let rereview = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    assert!(rereview.is_error.is_none(), "{}", extract_text(&rereview));

    let task_after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert!(task_after.get("review_feedback").is_none());
}

#[tokio::test]
async fn test_request_review_keeps_worker_assigned_and_preserves_review_owner() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Release worker for review"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "in_review");
    assert_eq!(task["assignee"], "worker-1");
    assert_eq!(task["review_owner"], "worker-1");
}

#[tokio::test]
async fn test_reseat_panel_recovers_collecting_review_without_lease() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Legacy collecting review reseat"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));

    let lease_path = env
        .root
        .join("runtime")
        .join("review-panels")
        .join(format!("{}.json", env.task_id));
    std::fs::remove_file(&lease_path).unwrap();

    let status_before = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_before_json = parse_result(&status_before);
    assert_eq!(status_before_json["action_needed"], "reseat_panel");
    assert_eq!(status_before_json["panel_lease"]["lease_state"], "missing");

    let reseat = tool
        .execute(serde_json::json!({
            "action": "reseat_panel",
            "task_id": env.task_id,
            "role": "supervisor"
        }))
        .await
        .unwrap();
    assert!(reseat.is_error.is_none(), "{}", extract_text(&reseat));
    let reseat_json = parse_result(&reseat);
    assert_eq!(reseat_json["reseated"], true);
    assert_eq!(reseat_json["panel_id"], "default-panel");
    assert!(lease_path.exists());

    let status_after = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_after_json = parse_result(&status_after);
    assert_eq!(
        status_after_json["panel_lease"]["lease_state"],
        "collecting"
    );
}

#[tokio::test]
async fn test_override_needs_revision_reopens_approved_task_for_rereview() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Approved task reopened for rereview",
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));
    let initial_review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    for reviewer in ["reviewer-alpha", "reviewer-beta"] {
        std::env::set_var("BREHON_AGENT_NAME", reviewer);
        std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
        let submit = tool
            .execute(serde_json::json!({
                "action": "submit_review",
                "review_id": initial_review_id,
                "reviewer": reviewer,
                "score": 8,
                "verdict": "approved",
                "summary": "looks good"
            }))
            .await
            .unwrap();
        assert!(submit.is_error.is_none(), "{}", extract_text(&submit));
    }

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let override_result = tool
        .execute(serde_json::json!({
            "action": "override",
            "task_id": env.task_id,
            "verdict": "needs_revision",
            "reason": "Rebased branch changed the reviewed commit"
        }))
        .await
        .unwrap();
    assert!(
        override_result.is_error.is_none(),
        "{}",
        extract_text(&override_result)
    );
    let override_json = parse_result(&override_result);
    assert_eq!(override_json["override_verdict"], "changes_requested");
    assert_eq!(override_json["requested_verdict"], "needs_revision");

    let status = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_json = parse_result(&status);
    assert_eq!(status_json["review_status"], "changes_requested");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "changes_requested");

    let rereview = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
        }))
        .await
        .unwrap();
    assert!(rereview.is_error.is_none(), "{}", extract_text(&rereview));
    let rereview_json = parse_result(&rereview);
    assert_eq!(rereview_json["round"], 2);
}

#[tokio::test]
async fn test_late_submission_after_supervisor_merge_is_ignored_without_tool_error() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();
    let task_tool = TaskActionsTool;

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Late merge handling",
        }))
        .await
        .unwrap();
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let first_submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 8,
            "verdict": "approved"
        }))
        .await
        .unwrap();
    assert!(
        first_submit.is_error.is_none(),
        "{}",
        extract_text(&first_submit)
    );

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let second_submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 9,
            "verdict": "approved",
            "summary": "approved by second reviewer"
        }))
        .await
        .unwrap();
    assert!(
        second_submit.is_error.is_none(),
        "{}",
        extract_text(&second_submit)
    );

    // Supervisor close runs verify_merge_ready which requires HEAD on
    // merge_target (main); init_git_workspace seeds workers on
    // worker/test.
    run_git(&env.workspace, &["checkout", "main"]);
    let close_result = task_tool
        .execute(serde_json::json!({
            "action": "close",
            "id": env.task_id,
            "role": "supervisor",
            "agent_name": "supervisor-1",
            "supervisor": "supervisor-1"
        }))
        .await
        .unwrap();
    assert!(
        close_result.is_error.is_none(),
        "{}",
        extract_text(&close_result)
    );

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let late_submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 9,
            "verdict": "approved",
            "summary": "late review after merge should be ignored"
        }))
        .await
        .unwrap();

    assert!(
        late_submit.is_error.is_none(),
        "late merged submit should be ignored without tool failure: {}",
        extract_text(&late_submit)
    );
    let late_json = parse_result(&late_submit);
    assert_eq!(late_json["ignored"], true);
    // A late submission after the task has been closed/merged surfaces the
    // more specific `task_closed` rejection rather than the generic
    // `missing_review_state` (see `RejectionReason::TaskClosed`).
    assert_eq!(late_json["reason"], "task_closed");
    assert_eq!(late_json["task_status"], "merged");
}

#[tokio::test]
async fn test_calibration_stats() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    // Run a complete review first to generate calibration data
    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
        }))
        .await
        .unwrap();
    let review_id = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    tool.execute(serde_json::json!({
        "action": "submit_review",
        "review_id": review_id,
        "score": 8,
        "verdict": "approved"
    }))
    .await
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    tool.execute(serde_json::json!({
        "action": "submit_review",
        "review_id": review_id,
        "score": 7,
        "verdict": "approved"
    }))
    .await
    .unwrap();

    // Now check calibration stats
    let result = tool
        .execute(serde_json::json!({
            "action": "calibration_stats"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let json = parse_result(&result);
    assert!(json["global_average"].as_f64().unwrap() >= 7.0);
    assert!(!json["reviewers"].as_array().unwrap().is_empty());

    // Check calibration file was written
    let cal_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join("calibration.json");
    assert!(cal_path.exists());
}

#[tokio::test]
async fn test_reassign_panel_swaps_dead_reviewers() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    // Request review — both reviewers alive
    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Test reassign",
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "request_review failed: {}",
        extract_text(&result)
    );
    let json = parse_result(&result);
    let panel = json["panel"].as_array().unwrap();
    assert_eq!(panel.len(), 2);

    // Simulate restart: reviewer-beta "dies" — remove its session file
    let sessions_dir = env.root.join("runtime").join("sessions");
    std::fs::remove_file(sessions_dir.join("reviewer-beta.json")).unwrap();

    // Add a new live reviewer to replace
    let new_session = serde_json::json!({
        "name": "reviewer-gamma",
        "role": "reviewer",
        "agent_type": "codex",
        "started_at": chrono::Utc::now().to_rfc3339()
    });
    std::fs::write(
        sessions_dir.join("reviewer-gamma.json"),
        serde_json::to_string_pretty(&new_session).unwrap(),
    )
    .unwrap();

    // Check review_status flags the dead reviewer
    let result = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let json = parse_result(&result);
    assert_eq!(json["action_needed"], "reassign_panel");
    let dead = json["dead_reviewers"].as_array().unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0], "reviewer-beta");

    // Now reassign panel
    let result = tool
        .execute(serde_json::json!({
            "action": "reassign_panel",
            "task_id": env.task_id,
            "requested_by": "supervisor-1"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "reassign_panel failed: {}",
        extract_text(&result)
    );
    let json = parse_result(&result);
    let new_panel: Vec<String> = json["panel"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    // reviewer-beta should be replaced with reviewer-gamma
    assert!(new_panel.contains(&"reviewer-alpha".to_string()));
    assert!(new_panel.contains(&"reviewer-gamma".to_string()));
    assert!(!new_panel.contains(&"reviewer-beta".to_string()));

    // Verify replacements info
    let replacements = json["replacements"].as_array().unwrap();
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0]["removed"], "reviewer-beta");
    assert_eq!(replacements[0]["replaced_with"], "reviewer-gamma");

    // Verify prompts were sent to new reviewer
    let prompts_sent: Vec<String> = json["prompts_sent_to"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(prompts_sent.contains(&"reviewer-gamma".to_string()));

    // Now reviewer-gamma can submit (verify they're accepted as panel member)
    let result = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": json["review_id"].as_str().unwrap(),
            "reviewer": "reviewer-gamma",
            "score": 8,
            "verdict": "approved",
            "summary": "Replacement reviewer approves"
        }))
        .await
        .unwrap();
    assert!(
        result.is_error.is_none(),
        "submission by replacement reviewer failed: {}",
        extract_text(&result)
    );
}

#[tokio::test]
async fn test_reassign_panel_no_dead_reviewers() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    tool.execute(serde_json::json!({
        "action": "request_review",
        "task_id": env.task_id,
        "title": "Test no-op reassign",
    }))
    .await
    .unwrap();

    // All reviewers still alive — reassign should be a no-op
    let result = tool
        .execute(serde_json::json!({
            "action": "reassign_panel",
            "task_id": env.task_id
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let json = parse_result(&result);
    assert!(json["message"]
        .as_str()
        .unwrap()
        .contains("No reassignment needed"));
}

#[tokio::test]
async fn test_consolidated_report_notifies_requester_without_live_supervisor_session() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    let sessions_dir = env.root.join("runtime").join("sessions");
    std::fs::remove_file(sessions_dir.join("supervisor-1.json")).unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Requester notification",
        }))
        .await
        .unwrap();
    let review_id = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    tool.execute(serde_json::json!({
        "action": "submit_review",
        "review_id": review_id,
        "score": 8,
        "verdict": "approved",
        "reviewer": "reviewer-alpha"
    }))
    .await
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    tool.execute(serde_json::json!({
        "action": "submit_review",
        "review_id": parse_result(&result)["review_id"],
        "score": 9,
        "verdict": "approved",
        "reviewer": "reviewer-beta"
    }))
    .await
    .unwrap();

    let messages = queued_messages_for(&env.root, "supervisor-1");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("Review complete for task")
                && message.contains("Outcome: APPROVED")),
        "requester should receive consolidated report even without a live supervisor session file"
    );
}

#[tokio::test]
async fn test_rereview_resets_round_timeout_clock() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let mut config = ReviewConfig::default();
    config.timeout_minutes = 1;
    config.default_reviewers = vec!["codex".to_string(), "gemini".to_string()];
    let tool = VerificationTool::new().with_config(config);

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Round one"
        }))
        .await
        .unwrap();
    let review_id_1 = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    tool.execute(serde_json::json!({
        "action": "submit_review",
        "review_id": review_id_1,
        "reviewer": "reviewer-alpha",
        "score": 8,
        "verdict": "approved"
    }))
    .await
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    tool.execute(serde_json::json!({
        "action": "submit_review",
        "review_id": review_id_1,
        "reviewer": "reviewer-beta",
        "score": 5,
        "verdict": "needs_revision"
    }))
    .await
    .unwrap();

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state["created_at"] =
        serde_json::json!((chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339());
    std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Round two"
        }))
        .await
        .unwrap();
    let json = parse_result(&result);
    assert_eq!(json["round"], 2);

    let state_after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_ne!(state_after["created_at"], state["created_at"]);

    let status = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_json = parse_result(&status);
    assert!(status_json.get("timed_out").is_none());
    assert_eq!(status_json["review_status"], "collecting");
}

#[tokio::test]
async fn test_timeout_recovery_starts_new_round_without_reusing_old_storage() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let mut config = ReviewConfig::default();
    config.timeout_minutes = 1;
    config.default_reviewers = vec!["codex".to_string(), "gemini".to_string()];
    let tool = VerificationTool::new().with_config(config);

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Timeout recovery"
        }))
        .await
        .unwrap();
    let first_review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state["created_at"] =
        serde_json::json!((chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339());
    std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    let timed_out = tool
        .execute(json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let timed_out_json = parse_result(&timed_out);
    assert_eq!(timed_out_json["review_status"], "escalated");
    assert_eq!(timed_out_json["action_needed"], "request_review");

    let second_request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Timeout recovery rerun"
        }))
        .await
        .unwrap();
    let second_json = parse_result(&second_request);
    let second_review_id = second_json["review_id"].as_str().unwrap().to_string();
    assert_ne!(first_review_id, second_review_id);
    assert_eq!(second_json["round"], 2);

    let round1_request = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("round-1")
        .join("request.json");
    let round2_request = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("round-2")
        .join("request.json");
    assert!(round1_request.exists());
    assert!(round2_request.exists());

    let round1_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(round1_request).unwrap()).unwrap();
    let round2_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(round2_request).unwrap()).unwrap();
    assert_eq!(round1_json["review_id"], first_review_id);
    assert_eq!(round2_json["review_id"], second_review_id);
}

#[tokio::test]
async fn test_timeout_with_partial_panel_does_not_evaluate_available_submissions() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.write_session("reviewer-gamma", "reviewer", Some("claude"));
    let mut config = ReviewConfig::default();
    config.timeout_minutes = 1;
    config.default_reviewers = vec![
        "codex".to_string(),
        "gemini".to_string(),
        "claude".to_string(),
    ];
    let tool = VerificationTool::new().with_config(config);

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Partial timeout"
        }))
        .await
        .unwrap();
    let request_json = parse_result(&request);
    let review_id = request_json["review_id"].as_str().unwrap().to_string();
    assert_eq!(request_json["panel"].as_array().unwrap().len(), 3);

    for reviewer in ["reviewer-alpha", "reviewer-beta"] {
        std::env::set_var("BREHON_AGENT_NAME", reviewer);
        std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
        let submit = tool
            .execute(json!({
                "action": "submit_review",
                "review_id": review_id,
                "reviewer": reviewer,
                "score": 9,
                "verdict": "approved",
                "summary": "looks good"
            }))
            .await
            .unwrap();
        assert!(submit.is_error.is_none(), "{}", extract_text(&submit));
    }

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state["created_at"] =
        serde_json::json!((chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339());
    std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
    let timed_out = tool
        .execute(json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let timed_out_json = parse_result(&timed_out);
    assert_eq!(timed_out_json["review_status"], "escalated");
    assert_eq!(timed_out_json["timed_out"], true);
    assert!(timed_out_json["message"]
        .as_str()
        .unwrap()
        .contains("incomplete quorum"));

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(task_path).unwrap()).unwrap();
    assert_eq!(task_json["status"], "in_review");
    let feedback = &task_json["review_feedback"];
    assert_eq!(feedback["outcome"], "escalated");
    assert_eq!(feedback["threshold_result"], "incomplete_quorum");
    assert_eq!(feedback["submitted_reviewers"].as_array().unwrap().len(), 2);
    assert_eq!(feedback["partial_submissions"].as_array().unwrap().len(), 2);
    assert!(feedback["pending_reviewers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "reviewer-gamma"));

    let consolidated = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("round-1")
        .join("consolidated.json");
    assert!(
        !consolidated.exists(),
        "partial timeout must not write a terminal consolidated review report"
    );
}

#[tokio::test]
async fn test_background_review_sweep_triggers_timeout_without_review_status_poll() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let mut config = ReviewConfig::default();
    config.timeout_minutes = 1;
    config.default_reviewers = vec!["codex".to_string(), "gemini".to_string()];
    let tool = VerificationTool::new().with_config(config);

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Background timeout"
        }))
        .await
        .unwrap();
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let mut state_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state_json["created_at"] =
        serde_json::json!((chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339());
    std::fs::write(
        &state_path,
        serde_json::to_string_pretty(&state_json).unwrap(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        ReviewMaintenanceAction::TimedOut {
            task_id,
            review_id: action_review_id,
            outcome,
        } => {
            assert_eq!(task_id, &env.task_id);
            assert_eq!(action_review_id, &review_id);
            assert_eq!(outcome, "escalated");
        }
        other => panic!("expected timed out maintenance action, got {other:?}"),
    }

    let state_after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state_after["status"], "escalated");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(task_path).unwrap()).unwrap();
    assert_eq!(task_json["status"], "changes_requested");
}

#[tokio::test]
async fn test_background_review_sweep_reassigns_dead_panel_members() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Background reassign"
        }))
        .await
        .unwrap();
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    env.remove_session("reviewer-beta");
    env.write_session("reviewer-gamma", "reviewer", Some("gemini"));

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        ReviewMaintenanceAction::ReassignedPanel {
            task_id,
            review_id: action_review_id,
            panel_id,
            replacements,
        } => {
            assert_eq!(task_id, &env.task_id);
            assert_eq!(action_review_id, &review_id);
            assert_eq!(panel_id, "default-panel");
            assert_eq!(replacements.len(), 1);
            assert_eq!(replacements[0].removed, "reviewer-beta");
            assert_eq!(replacements[0].replaced_with, "reviewer-gamma");
        }
        other => panic!("expected panel reassignment maintenance action, got {other:?}"),
    }

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let state_after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    let panel_after = state_after["panel"].as_array().unwrap();
    assert!(panel_after.iter().any(|value| value == "reviewer-gamma"));
    assert!(!panel_after.iter().any(|value| value == "reviewer-beta"));

    let queued = queued_messages_for(&env.root, "reviewer-gamma");
    assert!(
        queued.iter().any(|message| message.contains(&review_id)),
        "replacement reviewer should receive a fresh review prompt"
    );
}

#[tokio::test]
async fn test_background_review_sweep_recovers_orphaned_in_review_and_auto_requests_review() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task_json["status"] = "in_review".into();
    task_json["assignee"] = "worker-1".into();
    task_json["review_owner"] = "worker-1".into();
    task_json["latest_commit"] = run_git(&env.workspace, &["rev-parse", "HEAD"]).into();
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&task_json).unwrap(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(matches!(
        &actions[0],
        ReviewMaintenanceAction::RecoveredOrphanedGate {
            task_id,
            from_status,
            to_status,
        } if task_id == &env.task_id && from_status == "in_review" && to_status == "review_ready"
    ));
    assert!(matches!(
        &actions[1],
        ReviewMaintenanceAction::AutoRequestedReview { task_id, .. } if task_id == &env.task_id
    ));

    let repaired_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(repaired_task["status"], "in_review");
    assert_eq!(repaired_task["assignee"], "worker-1");
    assert_eq!(repaired_task["review_owner"], "worker-1");

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    assert!(state_path.exists(), "background sweep should seat a review");

    let reviewer_alpha_msgs = queued_messages_for(&env.root, "reviewer-alpha");
    let reviewer_beta_msgs = queued_messages_for(&env.root, "reviewer-beta");
    assert!(
        reviewer_alpha_msgs
            .iter()
            .any(|message| message.contains("Review request")),
        "{reviewer_alpha_msgs:?}"
    );
    assert!(
        reviewer_beta_msgs
            .iter()
            .any(|message| message.contains("Review request")),
        "{reviewer_beta_msgs:?}"
    );
}

#[tokio::test]
async fn test_background_review_sweep_recovers_orphaned_in_review_to_changes_requested() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task_json["status"] = "in_review".into();
    task_json["assignee"] = "worker-1".into();
    task_json["review_owner"] = "worker-1".into();
    task_json["blockers"] =
        "Reviewed commit still does not integrate cleanly. Checkpoint again and re-request review."
            .into();
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&task_json).unwrap(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(matches!(
        &actions[0],
        ReviewMaintenanceAction::RecoveredOrphanedGate {
            task_id,
            from_status,
            to_status,
        } if task_id == &env.task_id && from_status == "in_review" && to_status == "changes_requested"
    ));
    assert!(matches!(
        &actions[1],
        ReviewMaintenanceAction::ReleasedDeadWorkerAssignment {
            task_id,
            status,
            previous_assignee,
        } if task_id == &env.task_id
            && status == "changes_requested"
            && previous_assignee == "worker-1"
    ));

    let repaired_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(repaired_task["status"], "changes_requested");
    assert_eq!(repaired_task["assignee"], serde_json::Value::Null);

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    assert!(
        !state_path.exists(),
        "changes_requested recovery should not auto-seat a review"
    );
}

#[tokio::test]
async fn test_background_review_sweep_auto_requests_existing_review_ready_task() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task_json["status"] = "review_ready".into();
    task_json["assignee"] = "worker-1".into();
    task_json["review_owner"] = "worker-1".into();
    task_json["latest_commit"] = run_git(&env.workspace, &["rev-parse", "HEAD"]).into();
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&task_json).unwrap(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(matches!(
        &actions[0],
        ReviewMaintenanceAction::AutoRequestedReview { task_id, .. } if task_id == &env.task_id
    ));

    let repaired_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(repaired_task["status"], "in_review");
    assert_eq!(repaired_task["assignee"], "worker-1");

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    assert!(state_path.exists(), "review_ready task should enter review");
}

#[tokio::test]
async fn test_background_review_sweep_restores_collecting_review_when_task_status_drifted() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Preserve collecting review"
        }))
        .await
        .unwrap();
    assert!(request.is_error.is_none(), "{}", extract_text(&request));

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task_json["status"] = "changes_requested".into();
    task_json["assignee"] = serde_json::Value::Null;
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&task_json).unwrap(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert!(actions.is_empty(), "{actions:?}");

    let repaired_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(repaired_task["status"], "in_review");
    assert_eq!(repaired_task["assignee"], "worker-1");

    assert!(
        env.root
            .join("runtime")
            .join("reviews")
            .join(&env.task_id)
            .join("state.json")
            .exists(),
        "collecting review state should remain authoritative until the review finishes"
    );
    assert!(
        env.root
            .join("runtime")
            .join("review-panels")
            .join(format!("{}.json", env.task_id))
            .exists(),
        "active collecting review should keep its panel lease"
    );
}

#[tokio::test]
async fn test_background_review_sweep_clears_stale_review_state_before_auto_requesting_new_round() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task_json["status"] = "review_ready".into();
    task_json["assignee"] = "worker-1".into();
    task_json["review_owner"] = "worker-1".into();
    task_json["latest_commit"] = run_git(&env.workspace, &["rev-parse", "HEAD"]).into();
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&task_json).unwrap(),
    )
    .unwrap();

    let reviews_dir = env.root.join("runtime").join("reviews").join(&env.task_id);
    let round_one_dir = reviews_dir.join("round-1");
    std::fs::create_dir_all(&round_one_dir).unwrap();
    std::fs::write(
        reviews_dir.join("state.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": env.task_id,
            "status": "changes_requested",
            "current_round": 1,
            "cycle_start_round": 1,
            "current_review_id": "REV-old1111",
            "max_rounds": 3,
            "panel_id": "primary",
            "panel_mode": "configured_panel",
            "panel": ["reviewer-alpha", "reviewer-beta"],
            "submissions_received": ["reviewer-alpha", "reviewer-beta"],
            "created_at": "2026-04-02T00:00:00Z",
            "updated_at": "2026-04-02T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        round_one_dir.join("request.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": env.task_id,
            "review_id": "REV-old1111",
            "requested_by": "supervisor-1",
            "requested_at": "2026-04-02T00:00:00Z",
            "title": "Old review",
            "description": "Old round",
            "commit": run_git(&env.workspace, &["rev-parse", "HEAD"]),
            "base_commit": "",
            "merge_target_head": "",
            "commits": [],
            "context": ""
        }))
        .unwrap(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(matches!(
        &actions[0],
        ReviewMaintenanceAction::ReleasedStaleReviewState {
            task_id,
            review_id,
            review_status,
            task_status,
        } if task_id == &env.task_id
            && review_id == "REV-old1111"
            && review_status == "changes_requested"
            && task_status == "review_ready"
    ));
    assert!(matches!(
        &actions[1],
        ReviewMaintenanceAction::AutoRequestedReview { task_id, .. } if task_id == &env.task_id
    ));

    let state_path = reviews_dir.join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state["status"], "collecting");
    assert_eq!(
        state["current_round"], 2,
        "fresh review should advance past the highest round already on disk"
    );
    assert_ne!(state["current_review_id"], "REV-old1111");
    assert!(
        reviews_dir.join("round-2").join("request.json").exists(),
        "fresh review should write into the next round directory"
    );
}

#[tokio::test]
async fn test_background_review_sweep_share_after_submit_ignores_retained_lease_without_active_round(
) {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["codex".to_string(), "gemini".to_string()],
        panel_mode: ReviewPanelMode::FixedSize,
        lease_mode: ReviewLeaseMode::ShareAfterSubmit,
        panels: vec![ReviewPanelConfig {
            id: "primary".to_string(),
            reviewers: vec!["codex".to_string(), "gemini".to_string()],
        }],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let second_task = env.create_task_with_details(
        "Second task",
        "Should not be blocked by retained lease metadata",
        "worker-2",
        "merge",
    );

    let tasks_dir = env.root.join("runtime").join("tasks");
    let first_task_path = tasks_dir.join(format!("{}.json", env.task_id));
    let second_task_path = tasks_dir.join(format!("{second_task}.json"));
    let head = run_git(&env.workspace, &["rev-parse", "HEAD"]);

    let mut first_task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&first_task_path).unwrap()).unwrap();
    first_task_json["status"] = "changes_requested".into();
    first_task_json["assignee"] = serde_json::Value::Null;
    first_task_json["review_owner"] = "worker-1".into();
    first_task_json["latest_commit"] = head.clone().into();
    first_task_json["review_feedback"] = json!({
        "outcome": "changes_requested",
        "review_id": "REV-old1111"
    });
    std::fs::write(
        &first_task_path,
        serde_json::to_string_pretty(&first_task_json).unwrap(),
    )
    .unwrap();

    let mut second_task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&second_task_path).unwrap()).unwrap();
    second_task_json["status"] = "review_ready".into();
    second_task_json["assignee"] = "worker-2".into();
    second_task_json["review_owner"] = "worker-2".into();
    second_task_json["latest_commit"] = head.into();
    std::fs::write(
        &second_task_path,
        serde_json::to_string_pretty(&second_task_json).unwrap(),
    )
    .unwrap();

    let lease_dir = env.root.join("runtime").join("review-panels");
    std::fs::create_dir_all(&lease_dir).unwrap();
    std::fs::write(
        lease_dir.join(format!("{}.json", env.task_id)),
        serde_json::json!({
            "panel_id": "primary",
            "task_id": env.task_id,
            "review_id": "REV-old1111",
            "round": 1,
            "members": [
                { "slot_agent": "codex", "reviewer": "reviewer-alpha" },
                { "slot_agent": "gemini", "reviewer": "reviewer-beta" }
            ],
            "leased_at": chrono::Utc::now().to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339()
        })
        .to_string(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(matches!(
        &actions[0],
        ReviewMaintenanceAction::AutoRequestedReview { task_id, .. } if task_id == &second_task
    ));

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&second_task)
        .join("state.json");
    assert!(
        state_path.exists(),
        "review_ready task should enter review even when another task retains a stale lease"
    );
}

#[tokio::test]
async fn test_background_review_sweep_releases_dead_changes_requested_assignee() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let mut task_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    task_json["status"] = "changes_requested".into();
    task_json["assignee"] = "worker-1".into();
    task_json["review_owner"] = "worker-1".into();
    task_json["activity"] = "stale".into();
    std::fs::write(
        &task_path,
        serde_json::to_string_pretty(&task_json).unwrap(),
    )
    .unwrap();

    let actions = tool.sweep_collecting_reviews("supervisor-1").await;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(matches!(
        &actions[0],
        ReviewMaintenanceAction::ReleasedDeadWorkerAssignment {
            task_id,
            status,
            previous_assignee,
        } if task_id == &env.task_id && status == "changes_requested" && previous_assignee == "worker-1"
    ));

    let repaired_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(repaired_task["status"], "changes_requested");
    assert_eq!(repaired_task["assignee"], serde_json::Value::Null);
    assert_eq!(repaired_task["orphaned_assignee"], "worker-1");
    assert_eq!(repaired_task["orphaned_status"], "changes_requested");
    assert!(repaired_task.get("activity").is_none());
}

#[tokio::test]
async fn test_new_review_ignores_stale_submission_files_from_previous_review_id() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let request = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Stale submissions"
        }))
        .await
        .unwrap();
    let review_id = parse_result(&request)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    let round_dir = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("round-1");
    let stale_submission = serde_json::json!({
        "review_id": "REV-stale-old-review",
        "reviewer": "ghost-reviewer",
        "round": 1,
        "score": 1,
        "verdict": "rejected",
        "summary": "stale review should be ignored",
        "findings": [],
        "submitted_at": chrono::Utc::now().to_rfc3339()
    });
    std::fs::write(
        round_dir.join("ghost-reviewer.json"),
        serde_json::to_string_pretty(&stale_submission).unwrap(),
    )
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");
    let first_submit = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-alpha",
            "score": 8,
            "verdict": "approved"
        }))
        .await
        .unwrap();
    assert!(first_submit.is_error.is_none());

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    let second_submit = tool
        .execute(json!({
            "action": "submit_review",
            "review_id": review_id,
            "reviewer": "reviewer-beta",
            "score": 8,
            "verdict": "approved"
        }))
        .await
        .unwrap();
    assert!(
        second_submit.is_error.is_none(),
        "{}",
        extract_text(&second_submit)
    );

    let second_json = parse_result(&second_submit);
    assert_eq!(second_json["outcome"], "approved");
    assert_eq!(second_json["min_score"], 8);

    let consolidated_path = round_dir.join("consolidated.json");
    let consolidated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(consolidated_path).unwrap()).unwrap();
    let scores = consolidated["scores"].as_object().unwrap();
    assert_eq!(scores.len(), 2);
    assert!(scores.contains_key("reviewer-alpha"));
    assert!(scores.contains_key("reviewer-beta"));
    assert!(!scores.contains_key("ghost-reviewer"));
}

#[tokio::test]
async fn test_reassign_panel_refuses_to_shrink_frozen_council() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Frozen council"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));

    let sessions_dir = env.root.join("runtime").join("sessions");
    std::fs::remove_file(sessions_dir.join("reviewer-beta.json")).unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "reassign_panel",
            "task_id": env.task_id,
            "requested_by": "supervisor-1"
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(extract_text(&result).contains("Cannot preserve the frozen review council"));

    let status = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_json = parse_result(&status);
    assert_eq!(status_json["review_status"], "collecting");
    assert_eq!(status_json["action_needed"], "reassign_panel");
    assert_eq!(status_json["replacement_candidates"], serde_json::json!([]));
}

#[tokio::test]
async fn test_reassign_panel_updates_lease_slot_agent_to_match_cross_lane_replacement() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    env.remove_session("reviewer-alpha");
    env.remove_session("reviewer-beta");
    env.write_session("reviewer-claude", "reviewer", Some("claude-reviewer"));
    env.write_session("reviewer-codex-a", "reviewer", Some("codex-reviewer"));
    env.write_session("reviewer-codex-b", "reviewer", Some("codex-reviewer"));
    env.write_session("reviewer-gemini-a", "reviewer", Some("gemini-reviewer"));

    let tool = VerificationTool::new().with_config(ReviewConfig {
        policy: ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        },
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec![
            "claude-reviewer".to_string(),
            "codex-reviewer".to_string(),
            "gemini-reviewer".to_string(),
        ],
        panel_mode: ReviewPanelMode::FullCouncil,
        panels: vec![ReviewPanelConfig {
            id: "primary".to_string(),
            reviewers: vec![
                "claude-reviewer".to_string(),
                "codex-reviewer".to_string(),
                "gemini-reviewer".to_string(),
            ],
        }],
        ..ReviewConfig::default()
    });

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let requested = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Cross-lane reassignment"
        }))
        .await
        .unwrap();
    assert!(requested.is_error.is_none(), "{}", extract_text(&requested));

    env.remove_session("reviewer-gemini-a");

    let reassigned = tool
        .execute(json!({
            "action": "reassign_panel",
            "task_id": env.task_id,
            "requested_by": "supervisor-1"
        }))
        .await
        .unwrap();
    assert!(
        reassigned.is_error.is_none(),
        "{}",
        extract_text(&reassigned)
    );
    let reassigned_json = parse_result(&reassigned);
    assert_eq!(
        reassigned_json["replacements"],
        json!([{
            "removed": "reviewer-gemini-a",
            "replaced_with": "reviewer-codex-b"
        }])
    );

    let lease_path = env
        .root
        .join("runtime")
        .join("review-panels")
        .join(format!("{}.json", env.task_id));
    let lease_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lease_path).unwrap()).unwrap();
    let members = lease_json["members"].as_array().unwrap();
    let replacement = members
        .iter()
        .find(|member| member["reviewer"] == "reviewer-codex-b")
        .expect("replacement reviewer should be present in the lease");
    assert_eq!(replacement["slot_agent"], "codex-reviewer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repeated_review_cycles_cover_completion_reassignment_and_notifications() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let verification = std::sync::Arc::new(env.make_tool());
    let task_tool = TaskActionsTool;

    for cycle in 0..6 {
        let worker_name = format!("worker-{cycle}");
        let task_id = env.create_task_with_details(
            &format!("Stress cycle {cycle}"),
            &format!("Stress cycle {cycle} description"),
            &worker_name,
            "close",
        );

        let progress = task_tool
            .execute(json!({
                "action": "progress",
                "id": task_id,
                "percent": 100,
                "activity": "testing",
                "notes": format!("cycle {cycle} implementation complete"),
                "agent_name": worker_name,
                "role": "worker",
                "supervisor": "supervisor-1"
            }))
            .await
            .unwrap();
        assert!(
            progress.is_error.is_none(),
            "worker completion failed for cycle {cycle}: {}",
            extract_text(&progress)
        );
        let progress_json = parse_result(&progress);
        assert_eq!(progress_json["auto_review"], true);
        assert_eq!(progress_json["task_status"], "review_ready");

        let task_path = env
            .root
            .join("runtime")
            .join("tasks")
            .join(format!("{task_id}.json"));
        let task_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
        assert_eq!(task_json["status"], "review_ready");

        let supervisor_messages = queued_messages_for(&env.root, "supervisor-1");
        assert!(
            supervisor_messages.iter().any(|message| {
                message.contains(&format!("Task {task_id}")) && message.contains("ready for review")
            }),
            "worker completion should notify supervisor for task {task_id}"
        );

        let request = verification
            .execute(json!({
                "action": "request_review",
                "task_id": task_id,
                "requested_by": "supervisor-1",
                "title": format!("Stress cycle {cycle}"),
                "description": "Stress review coverage"
            }))
            .await
            .unwrap();
        assert!(
            request.is_error.is_none(),
            "request_review failed for cycle {cycle}: {}",
            extract_text(&request)
        );
        let request_json = parse_result(&request);
        let review_id = request_json["review_id"].as_str().unwrap().to_string();
        let panel = request_json["panel"].as_array().unwrap();
        assert_eq!(panel.len(), 2, "unexpected panel for cycle {cycle}");

        if cycle % 2 == 0 {
            let verification_a = verification.clone();
            let review_a = review_id.clone();
            let submit_a = tokio::spawn(async move {
                verification_a
                    .execute(json!({
                        "action": "submit_review",
                        "review_id": review_a,
                        "reviewer": "reviewer-alpha",
                        "score": 8,
                        "verdict": "approved",
                        "summary": "alpha approves concurrent cycle"
                    }))
                    .await
            });

            let verification_b = verification.clone();
            let review_b = review_id.clone();
            let submit_b = tokio::spawn(async move {
                verification_b
                    .execute(json!({
                        "action": "submit_review",
                        "review_id": review_b,
                        "reviewer": "reviewer-beta",
                        "score": 9,
                        "verdict": "approved",
                        "summary": "beta approves concurrent cycle"
                    }))
                    .await
            });

            let result_a = submit_a.await.unwrap().unwrap();
            let result_b = submit_b.await.unwrap().unwrap();
            assert!(
                result_a.is_error.is_none(),
                "concurrent alpha submit failed for cycle {cycle}: {}",
                extract_text(&result_a)
            );
            assert!(
                result_b.is_error.is_none(),
                "concurrent beta submit failed for cycle {cycle}: {}",
                extract_text(&result_b)
            );
        } else {
            let first_submit = verification
                .execute(json!({
                    "action": "submit_review",
                    "review_id": review_id,
                    "reviewer": "reviewer-alpha",
                    "score": 8,
                    "verdict": "approved",
                    "summary": "alpha approves before reassignment"
                }))
                .await
                .unwrap();
            assert!(
                first_submit.is_error.is_none(),
                "alpha submit failed for cycle {cycle}: {}",
                extract_text(&first_submit)
            );

            env.remove_session("reviewer-beta");
            let replacement = format!("reviewer-gamma-{cycle}");
            env.write_session(&replacement, "reviewer", Some("codex"));

            let status = verification
                .execute(json!({
                    "action": "review_status",
                    "task_id": task_id
                }))
                .await
                .unwrap();
            let status_json = parse_result(&status);
            assert_eq!(status_json["action_needed"], "reassign_panel");
            assert!(status_json["dead_reviewers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "reviewer-beta"));

            let reassign = verification
                .execute(json!({
                    "action": "reassign_panel",
                    "task_id": task_id,
                    "requested_by": "supervisor-1"
                }))
                .await
                .unwrap();
            assert!(
                reassign.is_error.is_none(),
                "reassign_panel failed for cycle {cycle}: {}",
                extract_text(&reassign)
            );
            let reassign_json = parse_result(&reassign);
            assert!(reassign_json["panel"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == &serde_json::json!(replacement)));
            assert!(reassign_json["prompts_sent_to"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == &serde_json::json!(replacement)));

            let replacement_messages = queued_messages_for(&env.root, &replacement);
            assert!(
                replacement_messages.iter().any(|message| {
                    message.contains(&review_id) && message.contains("panel reassignment")
                }),
                "replacement reviewer should receive reassignment prompt for task {task_id}"
            );

            let final_submit = verification
                .execute(json!({
                    "action": "submit_review",
                    "review_id": review_id,
                    "reviewer": replacement,
                    "score": 8,
                    "verdict": "approved",
                    "summary": "replacement reviewer approves"
                }))
                .await
                .unwrap();
            assert!(
                final_submit.is_error.is_none(),
                "replacement submit failed for cycle {cycle}: {}",
                extract_text(&final_submit)
            );

            env.remove_session(&format!("reviewer-gamma-{cycle}"));
            env.write_session("reviewer-beta", "reviewer", Some("gemini"));
        }

        let status = verification
            .execute(json!({
                "action": "review_status",
                "task_id": task_id
            }))
            .await
            .unwrap();
        let status_json = parse_result(&status);
        assert_eq!(status_json["review_status"], "approved");
        assert_eq!(status_json["progress"], "2/2");

        let task_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
        assert_eq!(task_json["status"], "approved");

        let consolidated_path = env
            .root
            .join("runtime")
            .join("reviews")
            .join(&task_id)
            .join("round-1")
            .join("consolidated.json");
        assert!(
            consolidated_path.exists(),
            "consolidated report missing for task {task_id}"
        );

        let supervisor_messages = queued_messages_for(&env.root, "supervisor-1");
        assert!(
            supervisor_messages.iter().any(|message| {
                message.contains(&format!("Review complete for task {task_id}"))
                    && message.contains("Outcome: APPROVED")
            }),
            "supervisor should receive consolidated report for task {task_id}"
        );

        std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
        std::env::set_var("BREHON_AGENT_ROLE", "supervisor");
        let close = task_tool
            .execute(json!({
                "action": "close",
                "id": task_id,
                "agent_name": "supervisor-1",
                "role": "supervisor",
                "supervisor": "supervisor-1"
            }))
            .await
            .unwrap();
        assert!(
            close.is_error.is_none(),
            "close failed for cycle {cycle}: {}",
            extract_text(&close)
        );
        let close_json = parse_result(&close);
        assert_eq!(close_json["released_panel"], "default-panel");

        let closed_task_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
        assert_eq!(closed_task_json["status"], "closed");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_review_submissions_do_not_lose_state() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = std::sync::Arc::new(env.make_tool());

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    let result = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "title": "Concurrent submit"
        }))
        .await
        .unwrap();
    let review_id = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    let tool_a = tool.clone();
    let review_a = review_id.clone();
    let submit_a = tokio::spawn(async move {
        tool_a
            .execute(json!({
                "action": "submit_review",
                "review_id": review_a,
                "reviewer": "reviewer-alpha",
                "score": 8,
                "verdict": "approved"
            }))
            .await
    });

    let tool_b = tool.clone();
    let review_b = review_id.clone();
    let submit_b = tokio::spawn(async move {
        tool_b
            .execute(json!({
                "action": "submit_review",
                "review_id": review_b,
                "reviewer": "reviewer-beta",
                "score": 9,
                "verdict": "approved"
            }))
            .await
    });

    let result_a = submit_a.await.unwrap().unwrap();
    let result_b = submit_b.await.unwrap().unwrap();
    assert!(
        result_a.is_error.is_none(),
        "submit A failed: {}",
        extract_text(&result_a)
    );
    assert!(
        result_b.is_error.is_none(),
        "submit B failed: {}",
        extract_text(&result_b)
    );

    let status = tool
        .execute(json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_json = parse_result(&status);
    assert_eq!(status_json["progress"], "2/2");
    assert_eq!(status_json["review_status"], "approved");

    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    let submitted = state["submissions_received"].as_array().unwrap();
    assert_eq!(submitted.len(), 2);
}

/// Test that state rebuild from persisted data is consistent.
/// Scenario: request review → persist state → simulate restart by reading state back
#[tokio::test]
async fn test_restart_recovery_rebuilds_consistent_state() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    // Request review and verify task status changed to in_review
    let result = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Restart recovery test"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let json = parse_result(&result);
    let review_id = json["review_id"].as_str().unwrap().to_string();

    // Verify task status is in_review
    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "in_review");

    // Verify review state persisted correctly
    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state["status"], "collecting");
    assert_eq!(state["current_review_id"], review_id);
    assert_eq!(state["current_round"], 1);
    assert!(state["panel"].as_array().unwrap().len() >= 2);

    // Simulate restart: read state back via review_status
    // This verifies that the rebuild logic can reconstruct from persisted data
    let status = tool
        .execute(json!({
            "action": "review_status",
            "task_id": env.task_id
        }))
        .await
        .unwrap();
    let status_json = parse_result(&status);
    assert_eq!(status_json["review_status"], "collecting");
    assert_eq!(status_json["round"], 1);
    assert_eq!(status_json["review_id"], review_id);
    assert!(status_json["panel"].as_array().unwrap().len() >= 2);

    // Verify task status matches review expectation (in_review)
    let task_after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task_after["status"], "in_review");

    // Verify round 1 request file exists
    let request_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("round-1")
        .join("request.json");
    assert!(request_path.exists());
    let request: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&request_path).unwrap()).unwrap();
    assert_eq!(request["review_id"], review_id);
}

/// Test that request_review handles failure correctly - no stranded reviewing state.
/// If we can't get a valid panel, we should NOT have left state on disk.
#[tokio::test]
async fn test_request_review_no_stranded_state_on_no_reviewers() {
    let _lock = ENV_LOCK.lock().unwrap();
    let workspace = std::env::temp_dir()
        .join("brehon-review-test")
        .join(format!("t-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workspace).unwrap();
    init_git_workspace(&workspace);
    let root = workspace.join(".brehon");
    std::fs::create_dir_all(&root).unwrap();
    let _env = ScopedBrehonEnv::set(&[
        ("BREHON_ROOT", root.as_os_str()),
        ("BREHON_WORKSPACE_ROOT", workspace.as_os_str()),
        ("BREHON_PROJECT_ROOT", OsStr::new("")),
        ("BREHON_WORKTREE_BRANCH", OsStr::new("")),
    ]);

    let task_id = format!(
        "T-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("test")
    );

    // Create task file
    let tasks_dir = root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let task = json!({
        "id": task_id,
        "task_id": task_id,
        "title": "No reviewers test",
        "description": "Test with no reviewers",
        "status": "in_progress",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1"
    });
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    // Create sessions dir but with NO reviewers
    let sessions_dir = root.join("runtime").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    // Only a supervisor, no reviewers
    let supervisor_session = json!({
        "name": "supervisor-1",
        "role": "supervisor",
        "agent_type": "claude"
    });
    std::fs::write(
        sessions_dir.join("supervisor-1.json"),
        serde_json::to_string(&supervisor_session).unwrap(),
    )
    .unwrap();

    let mut config = ReviewConfig::default();
    config.timeout_minutes = 60;
    config.panel_mode = ReviewPanelMode::FullCouncil;
    config.default_reviewers = vec!["codex".to_string(), "gemini".to_string()];
    let tool = VerificationTool::new().with_config(config);

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    // Request should fail because no reviewers available
    let result = tool
        .execute(json!({
            "action": "request_review",
            "task_id": task_id,
            "requested_by": "supervisor-1",
            "title": "No reviewers available"
        }))
        .await
        .unwrap();

    // Should get error about no reviewers
    assert!(
        result.is_error.is_some(),
        "Expected error with no reviewers"
    );
    let text = extract_text(&result);
    assert!(
        text.to_lowercase().contains("no reviewers") || text.to_lowercase().contains("reviewer"),
        "Error should mention reviewers: {text}"
    );

    // Verify NO review state was written (no stranded state)
    let review_path = root
        .join("runtime")
        .join("reviews")
        .join(&task_id)
        .join("state.json");
    assert!(
        !review_path.exists(),
        "Should not have stranded review state"
    );

    // Verify task status is still in_progress (not changed to in_review)
    let task_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tasks_dir.join(format!("{task_id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(task_json["status"], "in_progress");

    let _ = std::fs::remove_dir_all(&workspace);
}

/// Test that review→task state ordering is correct: task write happens before review state.
#[tokio::test]
async fn test_review_state_consistent_after_approved_review() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let tool = env.make_tool();

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    // Request review
    let result = tool
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Consistency test"
        }))
        .await
        .unwrap();
    let review_id = parse_result(&result)["review_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Submit approvals from both reviewers
    std::env::set_var("BREHON_AGENT_NAME", "reviewer-alpha");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");

    tool.execute(json!({
        "action": "submit_review",
        "review_id": review_id,
        "reviewer": "reviewer-alpha",
        "score": 9,
        "verdict": "approved",
        "summary": "Great"
    }))
    .await
    .unwrap();

    std::env::set_var("BREHON_AGENT_NAME", "reviewer-beta");
    tool.execute(json!({
        "action": "submit_review",
        "review_id": review_id,
        "reviewer": "reviewer-beta",
        "score": 8,
        "verdict": "approved",
        "summary": "Approve"
    }))
    .await
    .unwrap();

    // Verify consistency: review state approved AND task status approved
    let state_path = env
        .root
        .join("runtime")
        .join("reviews")
        .join(&env.task_id)
        .join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state["status"], "approved");

    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "approved");

    // Verify updated_at was set on task
    assert!(!task["updated_at"].as_str().unwrap().is_empty());
}

/// Test that concurrent close while review is collecting rejects the close.
#[tokio::test]
async fn test_close_during_active_review_rejected() {
    let _lock = ENV_LOCK.lock().unwrap();
    let env = TestEnv::new();
    let verification = env.make_tool();
    let task_tool = TaskActionsTool;

    std::env::set_var("BREHON_AGENT_NAME", "supervisor-1");
    std::env::set_var("BREHON_AGENT_ROLE", "supervisor");

    // Request review (task is now in_review)
    let result = verification
        .execute(json!({
            "action": "request_review",
            "task_id": env.task_id,
            "requested_by": "supervisor-1",
            "title": "Active review"
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{}", extract_text(&result));

    // Try to close the task while review is active
    let close_result = task_tool
        .execute(json!({
            "action": "close",
            "id": env.task_id,
            "agent_name": "supervisor-1",
            "role": "supervisor"
        }))
        .await
        .unwrap();

    // Close should be rejected because task is in_review
    assert!(
        close_result.is_error.is_some(),
        "Close should fail during active review"
    );
    let text = extract_text(&close_result);
    assert!(
        text.to_lowercase().contains("review") || text.to_lowercase().contains("in_review"),
        "Error should mention review state: {text}"
    );

    // Verify task is still in_review
    let task_path = env
        .root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", env.task_id));
    let task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(task["status"], "in_review");
}
