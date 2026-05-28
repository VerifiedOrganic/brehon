use super::*;
use crate::server::{ContentBlock, ToolResult};
use crate::tools::agent::prompt_queue_root;
use crate::tools::test_support::{
    write_pane_assignment_context_fixture, write_prompt_delivery_fixture,
};
use crate::tools::verification::helpers::workspace_root;
use crate::tools::verification::{panel, state};
use crate::tools::{ScopedEnv, TEST_ENV_LOCK};
use brehon_mux::{PromptQueueEntry, SessionScopedQueue};
use brehon_ports::ProofStore;
use brehon_store_fjall::FjallEventStore;
use brehon_types::{EventFilter, EventKind, TaskId};
use std::path::Path;
use std::sync::Arc;

fn make_tool() -> VerificationTool {
    VerificationTool::new()
}

fn run_git(workspace: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
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
    run_git(workspace, &["rev-parse", "HEAD"])
}

fn result_payload(result: &ToolResult) -> Value {
    match &result.content[0] {
        ContentBlock::Text { text } => serde_json::from_str(text)
            .unwrap_or_else(|err| panic!("tool result text should be JSON: {err}: {text}")),
        _ => unreachable!("tool result should contain text"),
    }
}

fn write_task_with_status(brehon_root: &Path, task_id: &str, status: &str) {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": task_id,
            "title": format!("Task {task_id}"),
            "description": "Submit-review rejection classification fixture",
            "status": status,
            "task_type": "task",
            "completion_mode": "merge",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn review_request_fixture(task_id: &str, review_id: &str) -> ReviewRequestFile {
    ReviewRequestFile {
        task_id: task_id.to_string(),
        review_id: review_id.to_string(),
        requested_by: "supervisor-1".to_string(),
        requested_at: chrono::Utc::now().to_rfc3339(),
        title: format!("Task {task_id}"),
        description: "Review request fixture".to_string(),
        commit: "abc123".to_string(),
        base_commit: String::new(),
        merge_target_head: String::new(),
        commits: Vec::new(),
        resolved_empty_commit_set: false,
        review_fingerprint: serde_json::json!({}),
        reviewer_prompts: std::collections::BTreeMap::new(),
        context: "Review context".to_string(),
    }
}

fn write_review_request_fixture(task_id: &str, review_id: &str) {
    state::write_round_request(task_id, 1, &review_request_fixture(task_id, review_id)).unwrap();
}

fn review_state_fixture(task_id: &str, review_id: &str, status: &str) -> ReviewState {
    ReviewState {
        task_id: task_id.to_string(),
        status: status.to_string(),
        current_round: 1,
        cycle_start_round: 1,
        review_epoch_start_round: 1,
        current_review_id: review_id.to_string(),
        max_rounds: 3,
        panel_id: "primary".to_string(),
        panel_mode: "full_council".to_string(),
        panel: vec!["reviewer-1".to_string()],
        submissions_received: Vec::new(),
        reviewer_assignments: std::collections::BTreeMap::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[test]
fn workspace_root_ignores_blank_workspace_root() {
    let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", "   "),
    ]);

    assert_eq!(workspace_root().as_deref(), Some(temp.path()));
}

#[test]
fn workspace_root_trims_workspace_root_value() {
    let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let wrapped = format!("  {}  ", workspace.path().display());
    let _env = ScopedEnv::set(&[("BREHON_WORKSPACE_ROOT", &wrapped)]);

    assert_eq!(workspace_root().as_deref(), Some(workspace.path()));
}

#[test]
fn test_build_panel_prefers_matching_types_then_falls_back() {
    let reviewers = vec![
        AgentInfo {
            name: "gemini-live".to_string(),
            agent_type: "gemini".to_string(),
        },
        AgentInfo {
            name: "codex-live".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "legacy-live".to_string(),
            agent_type: String::new(),
        },
    ];

    let panel = build_panel(&["codex".to_string()], &reviewers, 1);

    assert_eq!(panel[0], "codex-live");
    assert_eq!(panel.len(), 1);
}

#[test]
fn test_build_panel_deduplicates_duplicate_config_types() {
    let reviewers = vec![
        AgentInfo {
            name: "codex-a".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "codex-b".to_string(),
            agent_type: "codex".to_string(),
        },
    ];

    let panel = build_panel(&["codex".to_string(), "codex".to_string()], &reviewers, 2);

    assert_eq!(panel, vec!["codex-a".to_string(), "codex-b".to_string()]);
}

#[test]
fn test_find_agents_by_role_with_type_excludes_quarantined_agents() {
    let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("sessions")).unwrap();
    std::fs::create_dir_all(brehon_root.join("runtime").join("agent-health")).unwrap();

    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    crate::tools::agent::write_session_file(
        "reviewer-live",
        "reviewer",
        "sess-live",
        Some("gemini"),
    );
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("agent-health")
            .join("reviewer-live.json"),
        serde_json::json!({
            "agent": "reviewer-live",
            "status": "unavailable",
            "reason": "nonrecoverable_delivery_failure"
        })
        .to_string(),
    )
    .unwrap();

    let reviewers = find_agents_by_role_with_type("reviewer");
    assert!(reviewers.is_empty());
}

#[test]
fn test_build_panel_without_config_uses_requested_size() {
    let reviewers = vec![
        AgentInfo {
            name: "reviewer-a".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "reviewer-b".to_string(),
            agent_type: String::new(),
        },
    ];

    let panel = build_panel(&[], &reviewers, 1);

    assert_eq!(panel, vec!["reviewer-a".to_string()]);
}

#[test]
fn test_build_full_council_panel_uses_all_live_reviewers_when_unconfigured() {
    let reviewers = vec![
        AgentInfo {
            name: "reviewer-b".to_string(),
            agent_type: "gemini".to_string(),
        },
        AgentInfo {
            name: "reviewer-a".to_string(),
            agent_type: "codex".to_string(),
        },
    ];

    let panel = build_full_council_panel(&[], &reviewers);

    assert_eq!(
        panel,
        vec!["reviewer-a".to_string(), "reviewer-b".to_string()]
    );
}

#[test]
fn test_build_full_council_panel_uses_all_matching_reviewers() {
    let reviewers = vec![
        AgentInfo {
            name: "codex-b".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "claude-a".to_string(),
            agent_type: "claude".to_string(),
        },
        AgentInfo {
            name: "codex-a".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "gemini-a".to_string(),
            agent_type: "gemini".to_string(),
        },
    ];

    let panel = build_full_council_panel(&["codex".to_string(), "gemini".to_string()], &reviewers);

    assert_eq!(
        panel,
        vec![
            "codex-a".to_string(),
            "codex-b".to_string(),
            "gemini-a".to_string()
        ]
    );
}

#[test]
fn test_build_panel_caps_extra_live_reviewers() {
    let reviewers = vec![
        AgentInfo {
            name: "codex-a".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "codex-b".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "gemini-a".to_string(),
            agent_type: "gemini".to_string(),
        },
        AgentInfo {
            name: "claude-a".to_string(),
            agent_type: "claude".to_string(),
        },
    ];

    let panel = build_panel(&["codex".to_string(), "gemini".to_string()], &reviewers, 2);

    assert_eq!(panel, vec!["codex-a".to_string(), "gemini-a".to_string()]);
}

#[test]
fn test_build_panel_backfills_missing_configured_type() {
    let reviewers = vec![
        AgentInfo {
            name: "codex-a".to_string(),
            agent_type: "codex".to_string(),
        },
        AgentInfo {
            name: "claude-a".to_string(),
            agent_type: "claude".to_string(),
        },
    ];

    let panel = build_panel(&["codex".to_string(), "gemini".to_string()], &reviewers, 2);

    assert_eq!(panel, vec!["codex-a".to_string(), "claude-a".to_string()]);
}

#[tokio::test]
async fn test_submit_review_missing_review_id() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "submit_review",
        "verdict": "approved",
        "score": 8
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_submit_review_missing_verdict() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "submit_review",
        "review_id": "REV-001",
        "score": 8
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_submit_review_missing_score() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "submit_review",
        "review_id": "REV-001",
        "verdict": "approved"
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_submit_review_invalid_score() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "submit_review",
        "review_id": "REV-001",
        "verdict": "approved",
        "score": 11
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    if let ContentBlock::Text { text } = &result.content[0] {
        assert!(text.contains("1-10"));
    }
}

#[tokio::test]
async fn test_submit_review_score_zero() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "submit_review",
        "review_id": "REV-001",
        "verdict": "approved",
        "score": 0
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_submit_review_unknown_review_id_reports_typed_reason() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("reviews")).unwrap();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-does-not-exist",
            "verdict": "approved",
            "score": 8,
            "reviewer": "reviewer-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let payload = result_payload(&result);
    assert_eq!(payload["reason"], "unknown_review_id");
    assert_eq!(payload["review_id"], "REV-does-not-exist");
}

#[tokio::test]
async fn test_submit_review_missing_state_reports_missing_review_state_for_active_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task_with_status(&brehon_root, "T-missing-review-state", "in_review");
    write_review_request_fixture("T-missing-review-state", "REV-missing-state");

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-missing-state",
            "verdict": "approved",
            "score": 8,
            "reviewer": "reviewer-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, None);
    let payload = result_payload(&result);
    assert_eq!(payload["ignored"], true);
    assert_eq!(payload["reason"], "missing_review_state");
    assert_eq!(payload["task_status"], "in_review");
}

#[tokio::test]
async fn test_submit_review_missing_state_reports_task_closed_for_terminal_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task_with_status(&brehon_root, "T-closed-missing-state", "closed");
    write_review_request_fixture("T-closed-missing-state", "REV-closed-missing-state");

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-closed-missing-state",
            "verdict": "approved",
            "score": 8,
            "reviewer": "reviewer-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, None);
    let payload = result_payload(&result);
    assert_eq!(payload["ignored"], true);
    assert_eq!(payload["reason"], "task_closed");
    assert_eq!(payload["task_status"], "closed");
}

#[tokio::test]
async fn test_submit_review_superseded_review_reports_round_superseded() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task_with_status(&brehon_root, "T-superseded-review", "in_review");
    write_review_request_fixture("T-superseded-review", "REV-old");
    state::write_review_state(
        "T-superseded-review",
        &review_state_fixture("T-superseded-review", "REV-current", "collecting"),
    )
    .unwrap();

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-old",
            "verdict": "approved",
            "score": 8,
            "reviewer": "reviewer-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, None);
    let payload = result_payload(&result);
    assert_eq!(payload["ignored"], true);
    assert_eq!(payload["reason"], "round_superseded");
    assert_eq!(payload["active_review_id"], "REV-current");
}

#[tokio::test]
async fn test_submit_review_inactive_state_reports_round_superseded() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task_with_status(&brehon_root, "T-inactive-review", "in_review");
    write_review_request_fixture("T-inactive-review", "REV-inactive");
    state::write_review_state(
        "T-inactive-review",
        &review_state_fixture("T-inactive-review", "REV-inactive", "approved"),
    )
    .unwrap();

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-inactive",
            "verdict": "approved",
            "score": 8,
            "reviewer": "reviewer-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, None);
    let payload = result_payload(&result);
    assert_eq!(payload["ignored"], true);
    assert_eq!(payload["reason"], "round_superseded");
    assert_eq!(payload["review_status"], "approved");
}

#[tokio::test]
async fn test_submit_review_inactive_state_reports_task_closed_for_terminal_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task_with_status(&brehon_root, "T-merged-inactive-review", "merged");
    write_review_request_fixture("T-merged-inactive-review", "REV-merged-inactive");
    state::write_review_state(
        "T-merged-inactive-review",
        &review_state_fixture(
            "T-merged-inactive-review",
            "REV-merged-inactive",
            "approved",
        ),
    )
    .unwrap();

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-merged-inactive",
            "verdict": "approved",
            "score": 8,
            "reviewer": "reviewer-1"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, None);
    let payload = result_payload(&result);
    assert_eq!(payload["ignored"], true);
    assert_eq!(payload["reason"], "task_closed");
    assert_eq!(payload["task_status"], "merged");
}

#[tokio::test]
async fn test_request_review_missing_task_id() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "request_review"
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_request_review_rejects_reviewer_role() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_AGENT_ROLE", "reviewer")]);
    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-review"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = match &result.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("Only supervisors or Brehon maintenance can request reviews"));
}

#[tokio::test]
async fn test_request_review_recovers_blocked_integration_conflict_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-blocked-review.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-blocked-review",
            "title": "Blocked but reviewable",
            "description": "Supervisor repaired the merge issue",
            "status": "blocked",
            "task_type": "task",
            "completion_mode": "close",
            "assignee": null,
            "review_owner": null,
            "activity": "integration_conflict",
            "blockers": "Integration conflict for reviewed commit deadbeef against 'epic/test'.",
            "integration_conflict": {
                "owner": "supervisor",
                "source": "review_preflight",
                "merge_target": "epic/test",
                "reviewed_commit": "deadbeef",
                "previous_worker": "worker-1",
                "conflicting_files": ["src/lib.rs"]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-blocked-review"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );
    assert!(
        read_review_state("T-blocked-review").is_some(),
        "request_review should create review state"
    );
    let task: Value = serde_json::from_str(
        &std::fs::read_to_string(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join("T-blocked-review.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(task["status"], "in_review");
    assert_eq!(task["assignee"], "worker-1");
    assert_eq!(task["review_owner"], "worker-1");
    assert!(task.get("integration_conflict").is_none());
    assert!(task.get("blockers").is_none());
}

#[tokio::test]
async fn test_request_review_rejects_total_review_livelock() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-livelock.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-livelock",
            "title": "Runaway review task",
            "description": "This task has already exhausted review resets.",
            "status": "changes_requested",
            "task_type": "task",
            "completion_mode": "close",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();
    state::write_review_state(
        "T-livelock",
        &ReviewState {
            task_id: "T-livelock".to_string(),
            status: "changes_requested".to_string(),
            current_round: 9,
            cycle_start_round: 9,
            review_epoch_start_round: 1,
            current_review_id: "REV-livelock".to_string(),
            max_rounds: 3,
            panel_id: "primary".to_string(),
            panel_mode: "full_council".to_string(),
            panel: vec!["reviewer-1".to_string()],
            submissions_received: vec!["reviewer-1".to_string()],
            reviewer_assignments: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .unwrap();

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-livelock"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = match &result.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("review livelock"), "{text}");
    assert!(
        text.contains("Refusing to start/reset review round 10"),
        "{text}"
    );
}

#[tokio::test]
async fn test_reset_rounds_rejects_total_review_livelock() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-reset-livelock.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-reset-livelock",
            "title": "Runaway reset task",
            "description": "This task should not get another reset cycle.",
            "status": "changes_requested",
            "task_type": "task",
            "completion_mode": "close",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();
    state::write_review_state(
        "T-reset-livelock",
        &ReviewState {
            task_id: "T-reset-livelock".to_string(),
            status: "escalated".to_string(),
            current_round: 9,
            cycle_start_round: 7,
            review_epoch_start_round: 1,
            current_review_id: "REV-reset-livelock".to_string(),
            max_rounds: 3,
            panel_id: "primary".to_string(),
            panel_mode: "full_council".to_string(),
            panel: vec!["reviewer-1".to_string()],
            submissions_received: vec!["reviewer-1".to_string()],
            reviewer_assignments: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .unwrap();

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "reset_rounds",
            "task_id": "T-reset-livelock",
            "reason": "manual retry requested"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = match &result.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("review livelock"), "{text}");
    assert!(
        text.contains("Refusing to start/reset review round 10"),
        "{text}"
    );
}

#[tokio::test]
async fn test_reset_rounds_force_new_epoch_allows_manual_recovery() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-force-epoch.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-force-epoch",
            "title": "Manual epoch recovery",
            "description": "Non-commit work may need an explicit supervised epoch reset.",
            "status": "changes_requested",
            "task_type": "task",
            "completion_mode": "close",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();
    state::write_review_state(
        "T-force-epoch",
        &ReviewState {
            task_id: "T-force-epoch".to_string(),
            status: "escalated".to_string(),
            current_round: 9,
            cycle_start_round: 7,
            review_epoch_start_round: 1,
            current_review_id: "REV-force-epoch".to_string(),
            max_rounds: 3,
            panel_id: "primary".to_string(),
            panel_mode: "full_council".to_string(),
            panel: vec!["reviewer-1".to_string()],
            submissions_received: vec!["reviewer-1".to_string()],
            reviewer_assignments: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .unwrap();

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "reset_rounds",
            "task_id": "T-force-epoch",
            "reason": "Supervisor split the task and manually verified the implementation delta.",
            "force_new_epoch": true
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );
    let payload = result_payload(&result);
    assert_eq!(payload["next_round"], 10);
    assert_eq!(payload["force_new_epoch"], true);
    assert_eq!(payload["review_epoch_start_round"], 10);
    let state = read_review_state("T-force-epoch").expect("reset state");
    assert_eq!(state.status, "released");
    assert_eq!(state.review_epoch_start_round, 10);
}

#[tokio::test]
async fn test_request_review_rejects_new_commit_after_total_review_livelock() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());
    std::fs::write(workspace.path().join("feature.txt"), "old\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "old review payload"]);
    let old_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    std::fs::write(workspace.path().join("feature.txt"), "new\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "new review payload"]);
    let new_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-new-epoch.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-new-epoch",
            "title": "Restart epoch on real change",
            "description": "A new checkpoint should get a fresh review epoch.",
            "status": "changes_requested",
            "task_type": "task",
            "completion_mode": "merge",
            "assignee": "worker-1",
            "review_owner": "worker-1",
            "latest_commit": new_commit,
            "merge_target": "main"
        }))
        .unwrap(),
    )
    .unwrap();
    let mut prior_request = review_request_fixture("T-new-epoch", "REV-old");
    prior_request.commit = old_commit;
    state::write_round_request("T-new-epoch", 9, &prior_request).unwrap();
    state::write_review_state(
        "T-new-epoch",
        &ReviewState {
            task_id: "T-new-epoch".to_string(),
            status: "changes_requested".to_string(),
            current_round: 9,
            cycle_start_round: 9,
            review_epoch_start_round: 1,
            current_review_id: "REV-old".to_string(),
            max_rounds: 3,
            panel_id: "primary".to_string(),
            panel_mode: "full_council".to_string(),
            panel: vec!["reviewer-1".to_string()],
            submissions_received: vec!["reviewer-1".to_string()],
            reviewer_assignments: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .unwrap();

    let result = make_tool()
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-new-epoch"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = match &result.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("review livelock"), "{text}");
    assert!(
        text.contains("Refusing to start/reset review round 10"),
        "{text}"
    );
    let state = read_review_state("T-new-epoch").expect("new review state");
    assert_eq!(state.review_epoch_start_round, 1);
    assert_eq!(state.cycle_start_round, 9);
}

#[tokio::test]
async fn test_request_review_defaults_merge_mode_commit_to_head() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let head_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-merge-proof",
        "title": "Implement real merge gate",
        "description": "Code changes in task_actions.rs",
        "status": "in_progress",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-merge-proof.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-merge-proof",
            "title": "Implement real merge gate",
            "description": "Code changes in task_actions.rs"
        }))
        .await
        .unwrap();
    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request = read_round_request("T-merge-proof", 1).expect("request metadata should exist");
    assert_eq!(request.commit, head_commit);
    assert_eq!(request.base_commit, head_commit);
    assert_eq!(request.merge_target_head, head_commit);
    assert!(request.commits.is_empty());
    let reviewer_prompt = request
        .reviewer_prompts
        .get("reviewer-1")
        .expect("canonical prompt should be persisted per reviewer");
    assert!(reviewer_prompt.contains("Review fingerprint:"));
    assert!(reviewer_prompt.contains("Path interpretation:"));
    assert!(reviewer_prompt.contains("git log"));
}

#[tokio::test]
async fn test_request_review_uses_task_recorded_commit_when_commit_arg_missing() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let head_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-recorded-commit",
        "title": "Implement isolated worktrees",
        "description": "Code changes in run.rs",
        "status": "in_review",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": head_commit
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-recorded-commit.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-recorded-commit",
            "title": "Implement isolated worktrees",
            "description": "Code changes in run.rs"
        }))
        .await
        .unwrap();
    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );
    let result_json: serde_json::Value = match &result.content[0] {
        ContentBlock::Text { text } => serde_json::from_str(text).unwrap(),
        _ => unreachable!(),
    };
    assert_eq!(
        result_json["next_action"],
        serde_json::json!({
            "kind": "wait_for_reviews",
            "tool": "verification",
            "args": {
                "action": "review_status",
                "task_id": "T-recorded-commit"
            }
        })
    );
    assert_eq!(
        result_json["review_fingerprint"]["review_commit"],
        head_commit
    );
    assert_eq!(result_json["review_fingerprint"]["review_round"], 1);

    let request =
        read_round_request("T-recorded-commit", 1).expect("request metadata should exist");
    assert_eq!(request.commit, head_commit);
    assert_eq!(request.base_commit, head_commit);
    assert_eq!(request.merge_target_head, head_commit);
    assert!(request.commits.is_empty());
    assert_eq!(request.review_fingerprint["review_commit"], head_commit);
    assert_eq!(request.review_fingerprint["review_round"], 1);
    assert_eq!(request.review_fingerprint["diff_file_count"], 0);
}

#[tokio::test]
async fn test_request_review_rejects_stale_commit_arg_when_latest_commit_recorded() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    std::fs::write(workspace.path().join("feature.txt"), "old review payload\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "old review payload"]);
    let stale_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    std::fs::write(
        workspace.path().join("feature.txt"),
        "fresh review payload\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "fresh review payload"]);
    let latest_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-stale-review-commit",
        "title": "Reject stale review SHA",
        "description": "The task record has a newer checkpoint than the request arg.",
        "status": "review_ready",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": latest_commit,
        "merge_target": "main"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-stale-review-commit.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-stale-review-commit",
            "title": "Reject stale review SHA",
            "description": "The task record has a newer checkpoint than the request arg.",
            "commit": stale_commit
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = match &result.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("Refusing stale review commit"), "{text}");
    assert!(text.contains("authoritative latest_commit"), "{text}");
    assert!(
        read_review_state("T-stale-review-commit").is_none(),
        "stale commit request must not create review state"
    );
}

#[tokio::test]
async fn test_request_review_accepts_short_commit_arg_matching_latest_commit() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    std::fs::write(
        workspace.path().join("feature.txt"),
        "fresh review payload\n",
    )
    .unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "fresh review payload"]);
    let latest_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    let short_commit = latest_commit[..8].to_string();

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-short-review-commit",
        "title": "Accept matching abbreviated SHA",
        "description": "Short commit arg points at task.latest_commit.",
        "status": "review_ready",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": latest_commit,
        "merge_target": "main"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-short-review-commit.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-short-review-commit",
            "title": "Accept matching abbreviated SHA",
            "description": "Short commit arg points at task.latest_commit.",
            "commit": short_commit
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request =
        read_round_request("T-short-review-commit", 1).expect("request metadata should exist");
    assert_eq!(request.commit, task["latest_commit"].as_str().unwrap());
}

#[tokio::test]
async fn test_request_review_enriches_handoff_context_from_task_and_prior_feedback() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    std::fs::write(
        brehon_root.join("runtime").join("current-session.json"),
        serde_json::json!({
            "session_name": "brehon-review-handoff-test",
            "written_at": "2026-05-01T00:00:00Z"
        })
        .to_string(),
    )
    .unwrap();
    let head_commit = init_git_workspace(workspace.path());
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let reviewer_session_path = brehon_root
        .join("runtime")
        .join("sessions")
        .join("reviewer-1.json");
    let mut reviewer_session: Value =
        serde_json::from_str(&std::fs::read_to_string(&reviewer_session_path).unwrap()).unwrap();
    reviewer_session["session_name"] = Value::String("brehon-review-handoff-test".to_string());
    std::fs::write(
        &reviewer_session_path,
        serde_json::to_string_pretty(&reviewer_session).unwrap(),
    )
    .unwrap();
    let task = serde_json::json!({
        "task_id": "T-handoff-context",
        "title": "Encode NAS retry state",
        "description": "Persist retry state for non-3GPP access handover.",
        "status": "changes_requested",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "review_owner": "worker-1",
        "latest_commit": head_commit,
        "branch": "worker/non-3gpp-retry",
        "merge_target": "main",
        "notes": "Fixed the prior retry-state blocker and added regression coverage.",
        "acceptance_criteria": ["Retry state survives handover"],
        "file_hints": ["src/nas/retry.rs"],
        "test_requirements": ["cargo test -p nas retry_state"],
        "plan_steps": ["Patch retry state", "Add regression test"],
        "implementation_notes": "Keep IPv4 and IPv6 state independent.",
        "review_feedback": {
            "review_id": "REV-prior",
            "round": 1,
            "outcome": "changes_requested",
            "threshold_result": "changes_requested",
            "threshold_reason": "Blocking retry-state persistence gap",
            "blocking": [{
                "description": "Retry state is lost after handover",
                "file": "src/nas/retry.rs",
                "line": 42,
                "severity": "blocking",
                "suggestion": "Persist retry state before switching access."
            }],
            "suggestions": [],
            "nitpicks": [],
            "dissent": ["reviewer-2 wanted a narrower test"]
        }
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-handoff-context.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-handoff-context",
            "context": "Focus on the non-3GPP retry path."
        }))
        .await
        .unwrap();
    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request =
        read_round_request("T-handoff-context", 1).expect("request metadata should exist");
    assert_eq!(request.title, "Encode NAS retry state");
    assert_eq!(
        request.description,
        "Persist retry state for non-3GPP access handover."
    );
    assert!(request.context.contains("Supervisor context:"));
    assert!(request
        .context
        .contains("Focus on the non-3GPP retry path."));
    assert!(request.context.contains("Acceptance criteria:"));
    assert!(request.context.contains("Retry state survives handover"));
    assert!(request.context.contains("File hints:"));
    assert!(request.context.contains("src/nas/retry.rs"));
    assert!(request.context.contains("Test requirements:"));
    assert!(request.context.contains("cargo test -p nas retry_state"));
    assert!(request
        .context
        .contains("Previous review feedback to verify:"));
    assert!(request
        .context
        .contains("historical claims, not current evidence"));
    assert!(request.context.contains("exact review commit only"));
    assert!(request.context.contains("REV-prior"));
    assert!(request
        .context
        .contains("Retry state is lost after handover"));

    let stored_task: Value = serde_json::from_str(
        &std::fs::read_to_string(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join("T-handoff-context.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        stored_task.get("review_feedback").is_none(),
        "task feedback should still be cleared for the new active review"
    );

    let prompt_queue = SessionScopedQueue::<PromptQueueEntry>::new(
        "brehon-review-handoff-test",
        prompt_queue_root(&brehon_root),
    );
    let drained: Vec<_> = prompt_queue.drain().collect();
    let prompt = drained
        .into_iter()
        .find_map(|entry| {
            let entry = entry.ok()?;
            (entry.entry.target == "reviewer-1").then_some(entry.entry.message)
        })
        .expect("review prompt should be queued for reviewer-1");
    assert!(prompt.contains("Review handoff context:"));
    assert!(prompt.contains("Previous review feedback to verify:"));
    assert!(prompt.contains("Retry state is lost after handover"));
    assert!(prompt.contains("listed file/test hints as the starting scope"));
}

#[tokio::test]
async fn test_request_review_rejects_commit_that_conflicts_with_merge_target() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    std::fs::write(workspace.path().join("src.txt"), "base\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(workspace.path(), &["commit", "-m", "add src"]);

    run_git(workspace.path(), &["checkout", "-b", "epic/test"]);
    std::fs::write(workspace.path().join("src.txt"), "epic change\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    std::fs::write(workspace.path().join("src.txt"), "worker change\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker change"]);
    let worker_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    crate::tools::agent::write_session_file("worker-1", "worker", "worker-sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-conflicting-review",
        "title": "Conflicting merge candidate",
        "description": "Touches src.txt",
        "status": "in_progress",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": worker_commit,
        "merge_target": "epic/test"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-conflicting-review.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-conflicting-review",
            "title": "Conflicting merge candidate",
            "description": "Touches src.txt"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = match &result.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("does not integrate cleanly"));
    assert!(text.contains("src.txt"));
    assert!(read_review_state("T-conflicting-review").is_none());
    let task: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join("T-conflicting-review.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(task["status"], "changes_requested");
    // Worker assignment is preserved through the conflict so the worker
    // can rebase locally and re-request review.
    assert_eq!(task["assignee"], "worker-1");
    assert!(
        task.get("review_owner").is_none() || task["review_owner"].is_null(),
        "review_owner must be preserved (absent or null) when conflict is detected before review entered: {:?}",
        task.get("review_owner")
    );
    assert_eq!(task["activity"], "integration_conflict");
    // review_preflight conflicts are worker-fixable via rebase, so
    // ownership stays with the worker. Only approved_integration and
    // worker_unmerged conflicts default to supervisor ownership.
    assert_eq!(task["integration_conflict"]["owner"], "worker");
    assert_eq!(task["integration_conflict"]["source"], "review_preflight");
    assert_eq!(task["integration_conflict"]["previous_worker"], "worker-1");
    assert_eq!(
        task["integration_conflict"]["conflicting_files"][0],
        "src.txt"
    );
}

#[tokio::test]
async fn test_request_review_conflict_releases_stale_panel_lease() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    std::fs::write(workspace.path().join("src.txt"), "base\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(workspace.path(), &["commit", "-m", "add src"]);

    run_git(workspace.path(), &["checkout", "-b", "epic/test"]);
    std::fs::write(workspace.path().join("src.txt"), "epic change\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(workspace.path(), &["commit", "-m", "epic branch change"]);

    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    std::fs::write(workspace.path().join("src.txt"), "worker change\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(workspace.path(), &["commit", "-m", "worker change"]);
    let worker_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-conflicting-review",
        "title": "Conflicting merge candidate",
        "description": "Touches src.txt",
        "status": "changes_requested",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "review_owner": "worker-1",
        "latest_commit": worker_commit,
        "merge_target": "epic/test"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-conflicting-review.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    state::write_review_state(
        "T-conflicting-review",
        &ReviewState {
            task_id: "T-conflicting-review".to_string(),
            status: "collecting".to_string(),
            current_round: 2,
            cycle_start_round: 1,
            review_epoch_start_round: 1,
            current_review_id: "REV-stale".to_string(),
            max_rounds: 3,
            panel_id: "tertiary".to_string(),
            panel_mode: "full_council".to_string(),
            panel: vec!["reviewer-1".to_string()],
            submissions_received: Vec::new(),
            reviewer_assignments: std::collections::BTreeMap::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .unwrap();
    panel::write_panel_lease(&PanelLeaseState {
        panel_id: "tertiary".to_string(),
        task_id: "T-conflicting-review".to_string(),
        review_id: "REV-stale".to_string(),
        round: 2,
        members: vec![PanelLeaseMember {
            slot_agent: "codex".to_string(),
            reviewer: "reviewer-1".to_string(),
        }],
        leased_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-conflicting-review",
            "title": "Conflicting merge candidate",
            "description": "Touches src.txt"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let text = match &result.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    // Worker-facing error notes that the stale review panel was released.
    assert!(
        text.contains("Stale review panel tertiary was released")
            || text.contains("released panel tertiary"),
        "expected stale-panel release notice; got: {text}"
    );
    assert!(read_review_state("T-conflicting-review").is_none());
    assert!(find_panel_lease_by_task("T-conflicting-review").is_none());
}

#[tokio::test]
async fn test_request_review_records_full_reviewed_commit_set() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    std::fs::write(workspace.path().join("feature.txt"), "part 1\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "part 1"]);
    let first_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    std::fs::write(workspace.path().join("feature.txt"), "part 1\npart 2\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "part 2"]);
    let second_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    let main_head = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "worker/task"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-multi-commit-review",
        "title": "Multi-commit merge candidate",
        "description": "Touches feature.txt twice",
        "status": "in_progress",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": second_commit,
        "merge_target": "main"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-multi-commit-review.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-multi-commit-review",
            "title": "Multi-commit merge candidate",
            "description": "Touches feature.txt twice"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request =
        read_round_request("T-multi-commit-review", 1).expect("request metadata should exist");
    assert_eq!(request.commit, second_commit);
    assert_eq!(request.base_commit, main_head);
    assert_eq!(request.merge_target_head, main_head);
    assert_eq!(request.commits, vec![first_commit, second_commit]);
}

#[tokio::test]
async fn test_request_review_skips_empty_resubmission_commits_in_reviewed_set() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    run_git(
        workspace.path(),
        &["commit", "--allow-empty", "-m", "empty resubmission marker"],
    );
    let empty_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    std::fs::write(workspace.path().join("feature.txt"), "real change\n").unwrap();
    run_git(workspace.path(), &["add", "feature.txt"]);
    run_git(workspace.path(), &["commit", "-m", "real follow-up"]);
    let real_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    let main_head = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "worker/task"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-empty-resubmit-review",
        "title": "Merge candidate with empty resubmission marker",
        "description": "Has an empty checkpoint commit before a real change",
        "status": "review_ready",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": real_commit,
        "merge_target": "main"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-empty-resubmit-review.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-empty-resubmit-review",
            "title": "Merge candidate with empty resubmission marker",
            "description": "Has an empty checkpoint commit before a real change"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request =
        read_round_request("T-empty-resubmit-review", 1).expect("request metadata should exist");
    assert_eq!(request.commit, real_commit);
    assert_eq!(request.base_commit, main_head);
    assert_eq!(request.merge_target_head, main_head);
    assert_eq!(request.commits, vec![real_commit.clone()]);
    assert_ne!(empty_commit, real_commit);
}

#[tokio::test]
async fn test_request_review_treats_empty_tip_commit_as_noop_preflight() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    run_git(
        workspace.path(),
        &["commit", "--allow-empty", "-m", "empty reviewed tip"],
    );
    let empty_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    let main_head = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "worker/task"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-empty-tip-review",
        "title": "Merge candidate with empty reviewed tip",
        "description": "Latest reviewed commit is a no-op marker",
        "status": "review_ready",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": empty_commit,
        "merge_target": "main"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-empty-tip-review.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-empty-tip-review",
            "title": "Merge candidate with empty reviewed tip",
            "description": "Latest reviewed commit is a no-op marker"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request =
        read_round_request("T-empty-tip-review", 1).expect("request metadata should exist");
    assert_eq!(request.commit, empty_commit);
    assert_eq!(request.base_commit, main_head);
    assert_eq!(request.merge_target_head, main_head);
    assert!(request.resolved_empty_commit_set);
    assert!(request.commits.is_empty());
    assert!(read_review_state("T-empty-tip-review").is_some());
}

#[tokio::test]
async fn test_request_review_filters_already_applied_prior_commit_from_reviewed_set() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    std::fs::write(workspace.path().join("src.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "prior reviewed implementation"],
    );
    let _prior_reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    std::fs::write(workspace.path().join("src.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "src.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "merge target already has implementation"],
    );

    run_git(workspace.path(), &["checkout", "worker/task"]);
    std::fs::write(workspace.path().join("notes.md"), "follow-up delta\n").unwrap();
    run_git(workspace.path(), &["add", "notes.md"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "follow-up review delta"],
    );
    let followup_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-rereview-skip-empty-preflight",
        "title": "Re-review after already-applied base commit",
        "description": "Prior reviewed commit is already present on merge target; only the follow-up delta should block preflight",
        "status": "review_ready",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": followup_commit,
        "merge_target": "main"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-rereview-skip-empty-preflight.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-rereview-skip-empty-preflight",
            "title": "Re-review after already-applied base commit",
            "description": "Prior reviewed commit is already present on merge target; only the follow-up delta should block preflight"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request = read_round_request("T-rereview-skip-empty-preflight", 1)
        .expect("request metadata should exist");
    assert_eq!(request.commit, followup_commit);
    assert_eq!(request.commits, vec![followup_commit]);
}

#[tokio::test]
async fn test_request_review_keeps_ordered_chain_when_later_commit_reuses_prior_patch() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    init_git_workspace(workspace.path());

    run_git(workspace.path(), &["checkout", "-b", "worker/task"]);
    std::fs::write(workspace.path().join("foo.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "foo.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "initial add later duplicated upstream"],
    );
    let initial_add_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    std::fs::remove_file(workspace.path().join("foo.txt")).unwrap();
    run_git(workspace.path(), &["add", "foo.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "remove duplicated file"],
    );
    let remove_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    std::fs::write(workspace.path().join("foo.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "foo.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "re-add duplicated file after removal"],
    );
    let readd_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    run_git(workspace.path(), &["checkout", "main"]);
    std::fs::write(workspace.path().join("foo.txt"), "shared implementation\n").unwrap();
    run_git(workspace.path(), &["add", "foo.txt"]);
    run_git(
        workspace.path(),
        &["commit", "-m", "merge target already has the initial add"],
    );

    run_git(workspace.path(), &["checkout", "worker/task"]);

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_WORKSPACE_ROOT", workspace.path().to_str().unwrap()),
        ("BREHON_AGENT_NAME", "supervisor-1"),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    crate::tools::agent::write_session_file("reviewer-1", "reviewer", "sess-1", Some("codex"));
    let task = serde_json::json!({
        "task_id": "T-rereview-ordered-readd",
        "title": "Re-review preserves ordered replay semantics",
        "description": "A later commit intentionally reuses the same patch after an intermediate removal",
        "status": "review_ready",
        "task_type": "task",
        "completion_mode": "merge",
        "assignee": "worker-1",
        "latest_commit": readd_commit,
        "merge_target": "main"
    });
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-rereview-ordered-readd.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "request_review",
            "task_id": "T-rereview-ordered-readd",
            "title": "Re-review preserves ordered replay semantics",
            "description": "A later commit intentionally reuses the same patch after an intermediate removal"
        }))
        .await
        .unwrap();

    assert!(
        result.is_error.is_none(),
        "{}",
        match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    );

    let request =
        read_round_request("T-rereview-ordered-readd", 1).expect("request metadata should exist");
    assert_eq!(request.commit, readd_commit);
    assert_eq!(request.commits, vec![remove_commit, readd_commit.clone()]);
    assert_ne!(initial_add_commit, readd_commit);
}

#[tokio::test]
async fn test_review_status_missing_ids() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "review_status"
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_review_status_without_round_returns_request_review_hint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-ready-no-review.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-ready-no-review",
            "title": "Ready but not yet submitted",
            "description": "test fixture",
            "status": "review_ready",
            "task_type": "task",
            "completion_mode": "merge",
            "assignee": "worker-1"
        }))
        .unwrap(),
    )
    .unwrap();

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": "T-ready-no-review"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none());
    let payload: Value = serde_json::from_str(match &result.content[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("expected text block"),
    })
    .unwrap();
    assert_eq!(payload["review_status"], "not_requested");
    assert_eq!(payload["task_status"], "review_ready");
    assert_eq!(payload["action_needed"], "request_review");
    assert_eq!(payload["next_action"]["kind"], "request_review");
    assert_eq!(
        payload["next_action"]["args"]["task_id"],
        "T-ready-no-review"
    );
}

#[tokio::test]
async fn test_override_missing_params() {
    let tool = make_tool();
    let args = serde_json::json!({
        "action": "override",
        "task_id": "T-001"
    });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_review_status_surfaces_reviewer_assignment_observability() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    write_task_with_status(&brehon_root, "T-review-observe", "in_review");
    let mut state = review_state_fixture("T-review-observe", "REV-observe", "collecting");
    state.panel = vec!["reviewer-1".to_string()];
    state.reviewer_assignments.insert(
        "reviewer-1".to_string(),
        crate::tools::assignment_observability::AssignmentPropagation::new(
            "reviewer-1",
            "review",
            Some("prompt-reviewer-1".to_string()),
            Some("queued".to_string()),
        ),
    );
    state::write_review_state("T-review-observe", &state).unwrap();
    write_review_request_fixture("T-review-observe", "REV-observe");
    write_prompt_delivery_fixture(&brehon_root, "prompt-reviewer-1", "reviewer-1", true);
    write_pane_assignment_context_fixture(
        &brehon_root,
        "reviewer-1",
        "review",
        "T-review-observe",
        Some("REV-observe"),
        Some(1),
    );

    let tool = make_tool();
    let result = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": "T-review-observe"
        }))
        .await
        .unwrap();
    let payload = result_payload(&result);
    assert_eq!(
        payload["reviewer_assignments"][0]["assignment_observability"]["overall"],
        "delivered_without_ack"
    );
    assert_eq!(
        payload["reviewer_assignments"][0]["assignment_observability"]["active_context"]["matches"],
        true
    );
}

#[tokio::test]
async fn test_submit_review_acknowledges_reviewer_assignment_without_task_mine() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    write_task_with_status(&brehon_root, "T-submit-ack", "in_review");
    let mut state = review_state_fixture("T-submit-ack", "REV-submit-ack", "collecting");
    state.panel = vec!["reviewer-1".to_string(), "reviewer-2".to_string()];
    state.reviewer_assignments.insert(
        "reviewer-1".to_string(),
        crate::tools::assignment_observability::AssignmentPropagation::new(
            "reviewer-1",
            "review",
            Some("prompt-reviewer-1".to_string()),
            Some("queued".to_string()),
        ),
    );
    state.reviewer_assignments.insert(
        "reviewer-2".to_string(),
        crate::tools::assignment_observability::AssignmentPropagation::new(
            "reviewer-2",
            "review",
            Some("prompt-reviewer-2".to_string()),
            Some("queued".to_string()),
        ),
    );
    state::write_review_state("T-submit-ack", &state).unwrap();
    write_review_request_fixture("T-submit-ack", "REV-submit-ack");
    write_prompt_delivery_fixture(&brehon_root, "prompt-reviewer-1", "reviewer-1", true);
    write_pane_assignment_context_fixture(
        &brehon_root,
        "reviewer-1",
        "review",
        "T-submit-ack",
        Some("REV-submit-ack"),
        Some(1),
    );

    let tool = make_tool();
    let submit = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-submit-ack",
            "reviewer": "reviewer-1",
            "verdict": "approved",
            "score": 8,
            "summary": "Looks good"
        }))
        .await
        .unwrap();
    assert!(submit.is_error.is_none(), "{}", result_payload(&submit));

    let state = state::read_review_state("T-submit-ack").expect("review state should persist");
    let propagation = state
        .reviewer_assignments
        .get("reviewer-1")
        .expect("reviewer assignment should exist");
    assert_eq!(
        propagation.acknowledged_via.as_deref(),
        Some("verification action=submit_review")
    );
    assert_eq!(
        propagation.progress_started_via.as_deref(),
        Some("verification action=submit_review")
    );
    assert!(propagation.progress_started_at.is_some());

    let status = tool
        .execute(serde_json::json!({
            "action": "review_status",
            "task_id": "T-submit-ack"
        }))
        .await
        .unwrap();
    let payload = result_payload(&status);
    assert_eq!(
        payload["reviewer_assignments"][0]["assignment_observability"]["overall"],
        "active"
    );
    assert_eq!(
        payload["reviewer_assignments"][0]["assignment_observability"]["acknowledged_via"],
        "verification action=submit_review"
    );
    assert_eq!(
        payload["reviewer_assignments"][0]["assignment_observability"]["progress_started_via"],
        "verification action=submit_review"
    );
    assert_eq!(
        payload["reviewer_assignments"][0]["assignment_observability"]["progress_started"],
        true
    );
}

#[tokio::test]
async fn test_unknown_action() {
    let tool = make_tool();
    let args = serde_json::json!({ "action": "bogus" });
    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[test]
fn test_stored_finding_roundtrip() {
    let finding = StoredFinding {
        description: "Missing error handling".to_string(),
        file: Some("src/main.rs".to_string()),
        line: Some(42),
        severity: "blocking".to_string(),
        suggestion: Some("Use ? operator".to_string()),
    };
    let domain = finding.to_review_finding();
    assert_eq!(domain.description, "Missing error handling");
    assert_eq!(domain.severity, CommentSeverity::Blocking);
    assert!(domain.location.is_some());
    let loc = domain.location.as_ref().unwrap();
    assert_eq!(loc.file, "src/main.rs");
    assert_eq!(loc.line, 42);

    let back = StoredFinding::from_review_finding(&domain);
    assert_eq!(back.description, finding.description);
    assert_eq!(back.file, finding.file);
    assert_eq!(back.line, finding.line);
    assert_eq!(back.severity, finding.severity);
}

#[test]
fn test_parse_verdict() {
    assert_eq!(parse_verdict("approved"), ReviewVerdict::Approve);
    assert_eq!(
        parse_verdict("needs_revision"),
        ReviewVerdict::ChangesRequested
    );
    assert_eq!(
        parse_verdict("changes_requested"),
        ReviewVerdict::ChangesRequested
    );
    assert_eq!(parse_verdict("rejected"), ReviewVerdict::Reject);
}

#[test]
fn test_verdict_str() {
    assert_eq!(verdict_str(&ReviewVerdict::Approve), "approved");
    assert_eq!(
        verdict_str(&ReviewVerdict::ChangesRequested),
        "changes_requested"
    );
    assert_eq!(verdict_str(&ReviewVerdict::Reject), "rejected");
}

#[test]
fn test_evaluate_round_approved() {
    let tool = make_tool();
    let state = ReviewState {
        task_id: "T-001".to_string(),
        status: "collecting".to_string(),
        current_round: 1,
        cycle_start_round: 1,
        review_epoch_start_round: 1,
        current_review_id: "REV-001".to_string(),
        max_rounds: 3,
        panel_id: "primary".to_string(),
        panel_mode: "full_council".to_string(),
        panel: vec!["r1".to_string(), "r2".to_string()],
        submissions_received: vec!["r1".to_string(), "r2".to_string()],
        reviewer_assignments: std::collections::BTreeMap::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let submissions = vec![
        StoredSubmission {
            review_id: "REV-001".to_string(),
            reviewer: "r1".to_string(),
            round: 1,
            score: 8,
            verdict: "approved".to_string(),
            summary: "Good".to_string(),
            findings: vec![],
            submitted_at: String::new(),
        },
        StoredSubmission {
            review_id: "REV-001".to_string(),
            reviewer: "r2".to_string(),
            round: 1,
            score: 9,
            verdict: "approved".to_string(),
            summary: "Great".to_string(),
            findings: vec![],
            submitted_at: String::new(),
        },
    ];
    let report = tool.evaluate_round("T-001", "REV-001", &state, &submissions);
    assert_eq!(report.outcome, "approved");
    assert!(report.average_score >= 8.0);
}

#[test]
fn test_evaluate_round_changes_requested_blocking() {
    let tool = make_tool();
    let state = ReviewState {
        task_id: "T-001".to_string(),
        status: "collecting".to_string(),
        current_round: 1,
        cycle_start_round: 1,
        review_epoch_start_round: 1,
        current_review_id: "REV-001".to_string(),
        max_rounds: 3,
        panel_id: "primary".to_string(),
        panel_mode: "full_council".to_string(),
        panel: vec!["r1".to_string(), "r2".to_string()],
        submissions_received: vec!["r1".to_string(), "r2".to_string()],
        reviewer_assignments: std::collections::BTreeMap::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let submissions = vec![
        StoredSubmission {
            review_id: "REV-001".to_string(),
            reviewer: "r1".to_string(),
            round: 1,
            score: 8,
            verdict: "approved".to_string(),
            summary: String::new(),
            findings: vec![],
            submitted_at: String::new(),
        },
        StoredSubmission {
            review_id: "REV-001".to_string(),
            reviewer: "r2".to_string(),
            round: 1,
            score: 5,
            verdict: "needs_revision".to_string(),
            summary: String::new(),
            findings: vec![StoredFinding {
                description: "Bug".to_string(),
                file: Some("main.rs".to_string()),
                line: Some(10),
                severity: "blocking".to_string(),
                suggestion: None,
            }],
            submitted_at: String::new(),
        },
    ];
    let report = tool.evaluate_round("T-001", "REV-001", &state, &submissions);
    assert_eq!(report.outcome, "changes_requested");
    assert!(!report.blocking.is_empty());
}

#[test]
fn test_evaluate_round_rejected() {
    let tool = make_tool();
    let state = ReviewState {
        task_id: "T-001".to_string(),
        status: "collecting".to_string(),
        current_round: 1,
        cycle_start_round: 1,
        review_epoch_start_round: 1,
        current_review_id: "REV-001".to_string(),
        max_rounds: 3,
        panel_id: "primary".to_string(),
        panel_mode: "full_council".to_string(),
        panel: vec!["r1".to_string(), "r2".to_string()],
        submissions_received: vec!["r1".to_string(), "r2".to_string()],
        reviewer_assignments: std::collections::BTreeMap::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let submissions = vec![
        StoredSubmission {
            review_id: "REV-001".to_string(),
            reviewer: "r1".to_string(),
            round: 1,
            score: 8,
            verdict: "approved".to_string(),
            summary: String::new(),
            findings: vec![],
            submitted_at: String::new(),
        },
        StoredSubmission {
            review_id: "REV-001".to_string(),
            reviewer: "r2".to_string(),
            round: 1,
            score: 3,
            verdict: "rejected".to_string(),
            summary: String::new(),
            findings: vec![StoredFinding {
                description: "Fundamental correctness issue".to_string(),
                file: Some("main.rs".to_string()),
                line: Some(10),
                severity: "blocking".to_string(),
                suggestion: None,
            }],
            submitted_at: String::new(),
        },
    ];
    let report = tool.evaluate_round("T-001", "REV-001", &state, &submissions);
    assert_eq!(report.outcome, "rejected");
}

#[test]
fn test_evaluate_round_escalated_at_max_rounds() {
    let tool = make_tool();
    let state = ReviewState {
        task_id: "T-001".to_string(),
        status: "collecting".to_string(),
        current_round: 3, // at max
        cycle_start_round: 1,
        review_epoch_start_round: 1,
        current_review_id: "REV-003".to_string(),
        max_rounds: 3,
        panel_id: "primary".to_string(),
        panel_mode: "full_council".to_string(),
        panel: vec!["r1".to_string(), "r2".to_string()],
        submissions_received: vec!["r1".to_string(), "r2".to_string()],
        reviewer_assignments: std::collections::BTreeMap::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let submissions = vec![
        StoredSubmission {
            review_id: "REV-003".to_string(),
            reviewer: "r1".to_string(),
            round: 3,
            score: 8,
            verdict: "approved".to_string(),
            summary: String::new(),
            findings: vec![],
            submitted_at: String::new(),
        },
        StoredSubmission {
            review_id: "REV-003".to_string(),
            reviewer: "r2".to_string(),
            round: 3,
            score: 5,
            verdict: "needs_revision".to_string(),
            summary: String::new(),
            findings: vec![StoredFinding {
                description: "Still requires a concrete rework pass".to_string(),
                file: Some("main.rs".to_string()),
                line: Some(10),
                severity: "blocking".to_string(),
                suggestion: None,
            }],
            submitted_at: String::new(),
        },
    ];
    let report = tool.evaluate_round("T-001", "REV-003", &state, &submissions);
    assert_eq!(report.outcome, "escalated");
    assert!(report.threshold_reason.contains("Max review rounds"));
}

#[tokio::test]
async fn test_submit_review_approval_preflight_rejects_dirty_shared_root_and_allows_retry_after_cleanup(
) {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    init_git_workspace(root.path());
    std::fs::write(root.path().join("leaked.txt"), "wrong tree\n").unwrap();

    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
    ]);

    write_task_with_status(&brehon_root, "T-dirty-approve", "in_review");
    let mut state = review_state_fixture("T-dirty-approve", "REV-dirty-approve", "collecting");
    state.panel = vec!["reviewer-1".to_string(), "reviewer-2".to_string()];
    state::write_review_state("T-dirty-approve", &state).unwrap();
    write_review_request_fixture("T-dirty-approve", "REV-dirty-approve");

    let tool = make_tool();

    // reviewer-1 submits first — panel not yet complete, so no dirty check
    let r1 = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-dirty-approve",
            "reviewer": "reviewer-1",
            "verdict": "approved",
            "score": 8,
            "summary": "Looks good"
        }))
        .await
        .unwrap();
    assert!(r1.is_error.is_none(), "{}", result_payload(&r1));

    // reviewer-2 submits approval — this would complete the panel, so dirty
    // root preflight should reject BEFORE persisting the submission.
    let r2 = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-dirty-approve",
            "reviewer": "reviewer-2",
            "verdict": "approved",
            "score": 9,
            "summary": "Also looks good"
        }))
        .await
        .unwrap();
    assert_eq!(r2.is_error, Some(true));
    let text = match &r2.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text content"),
    };
    assert!(
        text.contains("shared repo root"),
        "expected dirty-root error, got: {text}"
    );
    assert!(
        text.contains("leaked.txt"),
        "expected leaked.txt in error, got: {text}"
    );
    assert!(
        text.contains("Recovery:"),
        "expected recovery hint, got: {text}"
    );

    // reviewer-2 must NOT have been recorded as submitted
    let state_after =
        state::read_review_state("T-dirty-approve").expect("review state should exist");
    assert!(
        !state_after
            .submissions_received
            .iter()
            .any(|s| s == "reviewer-2"),
        "reviewer-2 should not be recorded after dirty-root rejection"
    );

    // Task must still be in_review
    let task = read_task("T-dirty-approve").expect("task should exist");
    assert_eq!(task["status"], "in_review");

    // Clean the dirty root
    std::fs::remove_file(root.path().join("leaked.txt")).unwrap();

    // reviewer-2 retries — should now succeed and approve the task
    let r2_retry = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-dirty-approve",
            "reviewer": "reviewer-2",
            "verdict": "approved",
            "score": 9,
            "summary": "Also looks good"
        }))
        .await
        .unwrap();
    assert!(r2_retry.is_error.is_none(), "{}", result_payload(&r2_retry));

    let task_after = read_task("T-dirty-approve").expect("task should exist");
    assert_eq!(task_after["status"], "approved");
}

#[tokio::test]
async fn test_rollback_review_submission() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    let task_id = "T-rollback-unit";
    write_task_with_status(&brehon_root, task_id, "in_review");

    let mut state = review_state_fixture(task_id, "REV-rollback-unit", "collecting");
    state.panel = vec!["reviewer-1".to_string(), "reviewer-2".to_string()];
    state.submissions_received.push("reviewer-1".to_string());
    state::write_review_state(task_id, &state).unwrap();
    write_review_request_fixture(task_id, "REV-rollback-unit");

    // Persist reviewer-2's submission file and state entries
    let submission = StoredSubmission {
        review_id: "REV-rollback-unit".to_string(),
        reviewer: "reviewer-2".to_string(),
        round: 1,
        score: 9,
        verdict: "approved".to_string(),
        summary: "Looks good".to_string(),
        findings: vec![],
        submitted_at: chrono::Utc::now().to_rfc3339(),
    };
    state::write_submission(task_id, 1, "reviewer-2", &submission).unwrap();
    state.submissions_received.push("reviewer-2".to_string());
    state.reviewer_assignments.insert(
        "reviewer-2".to_string(),
        crate::tools::assignment_observability::AssignmentPropagation::new(
            "reviewer-2",
            "review",
            Some("prompt-2".to_string()),
            Some("delivered".to_string()),
        ),
    );
    state::write_review_state(task_id, &state).unwrap();

    let round_dir = brehon_root
        .join("runtime")
        .join("reviews")
        .join(task_id)
        .join("round-1");

    // Preconditions
    let pre = state::read_review_state(task_id).unwrap();
    assert!(pre.submissions_received.contains(&"reviewer-2".to_string()));
    assert!(pre.reviewer_assignments.contains_key("reviewer-2"));
    assert!(round_dir.join("reviewer-2.json").exists());

    // Roll back reviewer-2
    let mut rollback_state = pre.clone();
    let _ = state::rollback_review_submission(task_id, 1, "reviewer-2", &mut rollback_state);

    // Postconditions
    let post = state::read_review_state(task_id).unwrap();
    assert!(
        !post
            .submissions_received
            .contains(&"reviewer-2".to_string()),
        "reviewer-2 should be removed from submissions_received"
    );
    assert!(
        !post.reviewer_assignments.contains_key("reviewer-2"),
        "reviewer-2 assignment should be removed"
    );
    assert!(
        post.submissions_received
            .contains(&"reviewer-1".to_string()),
        "reviewer-1 should remain"
    );
    assert!(
        !round_dir.join("reviewer-2.json").exists(),
        "reviewer-2 submission file should be deleted"
    );
}

#[tokio::test]
async fn test_submit_review_rolls_back_when_status_update_rejects_dirty_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    init_git_workspace(root.path());

    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", root.path().to_str().unwrap()),
    ]);

    write_task_with_status(&brehon_root, "T-dirty-post-persist", "in_review");
    let mut state = review_state_fixture(
        "T-dirty-post-persist",
        "REV-dirty-post-persist",
        "collecting",
    );
    state.panel = vec!["reviewer-1".to_string(), "reviewer-2".to_string()];
    state::write_review_state("T-dirty-post-persist", &state).unwrap();
    write_review_request_fixture("T-dirty-post-persist", "REV-dirty-post-persist");

    // Attach a real proof store so we can verify no duplicate entries on retry.
    let fjall = Arc::new(FjallEventStore::new(brehon_root.join("fjall")).unwrap());
    let proof_store: Arc<dyn ProofStore + Send + Sync> = fjall.clone();
    let tool = make_tool()
        .with_event_store(fjall.clone())
        .with_proof_store(proof_store.clone());

    // reviewer-1 submits first — panel not yet complete
    let r1 = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-dirty-post-persist",
            "reviewer": "reviewer-1",
            "verdict": "approved",
            "score": 8,
            "summary": "Looks good"
        }))
        .await
        .unwrap();
    assert!(r1.is_error.is_none(), "{}", result_payload(&r1));

    // Force the dirty-root check inside update_task_status_atomic to fail.
    // Invocation sequence for reviewer-2 (2-person panel, approved):
    //   0 = preflight in handle_submit_review
    //   1 = second check in handle_submit_review
    //   2 = check inside update_task_status_atomic
    crate::tools::task_actions::reset_test_dirty_check_invocation();
    crate::tools::task_actions::set_test_force_dirty_on_invocation(2);

    // reviewer-2 submits approval — this would complete the panel, but the
    // dirty check inside update_task_status_atomic fails, so the submission
    // must be rolled back.
    let r2 = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-dirty-post-persist",
            "reviewer": "reviewer-2",
            "verdict": "approved",
            "score": 9,
            "summary": "Also looks good"
        }))
        .await
        .unwrap();

    // Clean up the test hook immediately so it doesn't leak to other tests
    crate::tools::task_actions::clear_test_force_dirty_on_invocation();

    assert_eq!(r2.is_error, Some(true));
    let text = match &r2.content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text content"),
    };
    assert!(
        text.contains("forced by test hook") || text.contains("Failed to update task"),
        "expected dirty-root rejection inside status update, got: {text}"
    );

    // reviewer-2 must NOT have been recorded as submitted after rollback
    let state_after =
        state::read_review_state("T-dirty-post-persist").expect("review state should exist");
    assert!(
        !state_after
            .submissions_received
            .iter()
            .any(|s| s == "reviewer-2"),
        "reviewer-2 should not be recorded after rollback"
    );

    // reviewer-2's submission file must be gone
    let round_dir = brehon_root
        .join("runtime")
        .join("reviews")
        .join("T-dirty-post-persist")
        .join("round-1");
    assert!(
        !round_dir.join("reviewer-2.json").exists(),
        "reviewer-2 submission file should be deleted after rollback"
    );
    assert!(
        !round_dir.join("consolidated.json").exists(),
        "consolidated report should be deleted after rollback"
    );

    // Task must still be in_review
    let task = read_task("T-dirty-post-persist").expect("task should exist");
    assert_eq!(task["status"], "in_review");

    // Proof bundle must NOT exist after rollback — record_consolidation is now
    // deferred until after update_task_status_atomic succeeds.
    let bundle_after_rollback = proof_store
        .proof_bundle_for_task(&TaskId::new("T-dirty-post-persist"))
        .await
        .unwrap();
    assert!(
        bundle_after_rollback.is_none(),
        "proof bundle should not exist after rollback"
    );

    // reviewer-2 retries (without the test hook) — should now succeed and approve the task
    let r2_retry = tool
        .execute(serde_json::json!({
            "action": "submit_review",
            "review_id": "REV-dirty-post-persist",
            "reviewer": "reviewer-2",
            "verdict": "approved",
            "score": 9,
            "summary": "Also looks good"
        }))
        .await
        .unwrap();
    assert!(r2_retry.is_error.is_none(), "{}", result_payload(&r2_retry));

    let task_after = read_task("T-dirty-post-persist").expect("task should exist");
    assert_eq!(task_after["status"], "approved");

    // Proof bundle must exist after successful retry and contain exactly the
    // two reviewers' scores — no duplicates from the rolled-back attempt.
    let bundle = proof_store
        .proof_bundle_for_task(&TaskId::new("T-dirty-post-persist"))
        .await
        .unwrap()
        .expect("proof bundle should exist after successful retry");
    assert_eq!(
        bundle.review_scores.len(),
        2,
        "proof bundle should contain exactly 2 review scores, not duplicates from rollback: {:?}",
        bundle.review_scores
    );
    let reviewer_ids: Vec<String> = bundle
        .review_scores
        .iter()
        .filter_map(|review| review.reviewer_id.clone())
        .collect();
    assert!(reviewer_ids.contains(&"reviewer-1".to_string()));
    assert!(reviewer_ids.contains(&"reviewer-2".to_string()));

    // ReviewScoreReceived must have been emitted exactly twice overall
    // (once for reviewer-1 on incomplete panel, once for reviewer-2 after
    // successful completion), with no duplicate from the rolled-back attempt.
    let all_events = fjall
        .query(EventFilter::new().aggregate("REV-dirty-post-persist"))
        .await
        .unwrap();
    let score_events: Vec<_> = all_events
        .into_iter()
        .filter(|e| matches!(e.kind, EventKind::ReviewScoreReceived { .. }))
        .collect();
    assert_eq!(
        score_events.len(),
        2,
        "expected exactly 2 ReviewScoreReceived events, got {:?}",
        score_events
    );
}

#[test]
fn test_evaluate_round_escalates_at_total_review_livelock_limit() {
    let tool = make_tool();
    let state = ReviewState {
        task_id: "T-livelock".to_string(),
        status: "collecting".to_string(),
        current_round: 9,
        cycle_start_round: 9,
        review_epoch_start_round: 1,
        current_review_id: "REV-livelock".to_string(),
        max_rounds: 3,
        panel_id: "primary".to_string(),
        panel_mode: "full_council".to_string(),
        panel: vec!["r1".to_string(), "r2".to_string()],
        submissions_received: vec!["r1".to_string(), "r2".to_string()],
        reviewer_assignments: std::collections::BTreeMap::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let submissions = vec![
        StoredSubmission {
            review_id: "REV-livelock".to_string(),
            reviewer: "r1".to_string(),
            round: 9,
            score: 8,
            verdict: "approved".to_string(),
            summary: String::new(),
            findings: vec![],
            submitted_at: String::new(),
        },
        StoredSubmission {
            review_id: "REV-livelock".to_string(),
            reviewer: "r2".to_string(),
            round: 9,
            score: 5,
            verdict: "needs_revision".to_string(),
            summary: String::new(),
            findings: vec![StoredFinding {
                description: "Still requires a concrete rework pass".to_string(),
                file: Some("main.rs".to_string()),
                line: Some(10),
                severity: "blocking".to_string(),
                suggestion: None,
            }],
            submitted_at: String::new(),
        },
    ];

    let report = tool.evaluate_round("T-livelock", "REV-livelock", &state, &submissions);

    assert_eq!(report.outcome, "escalated");
    assert!(
        report.threshold_reason.contains("Total review round limit"),
        "{}",
        report.threshold_reason
    );
}
