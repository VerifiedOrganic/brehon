use super::*;
use crate::server::ContentBlock;
use crate::tools::TEST_ENV_LOCK;
use brehon_ports::{EventStore, RunStore};
use brehon_store_fjall::FjallEventStore;
use brehon_types::{ClaimGeneration, ClaimOwner, RunId, RunRole, RunStatus, SessionId};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

struct ScopedEnv {
    vars: Vec<(String, Option<OsString>)>,
}

impl ScopedEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut stored = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            stored.push((key.to_string(), std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { vars: stored }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.vars.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn write_task(brehon_root: &Path, task: &serde_json::Value) {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    let id = task
        .get("task_id")
        .and_then(|value| value.as_str())
        .unwrap_or("TASK-001");
    let path = tasks_dir.join(format!("{id}.json"));
    fs::write(path, serde_json::to_string_pretty(task).unwrap()).unwrap();
}

fn setup_brehon_root() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let brehon_root = temp.path().join(".brehon");
    fs::create_dir_all(&brehon_root.join("runtime").join("tasks")).unwrap();
    (temp, brehon_root)
}

fn enable_context_compression(brehon_root: &Path) {
    fs::write(
        brehon_root.join("config.yaml"),
        "version: 1\ncontext:\n  compression:\n    enabled: true\n",
    )
    .unwrap();
}

#[tokio::test]
async fn test_get_task_context_tool() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-0001",
            "title": "Auth middleware",
            "description": "Add auth middleware support.",
            "status": "in_progress",
            "priority": "High",
            "assignee": "worker-1",
            "dependencies": ["T-base-1"],
            "file_hints": ["src/middleware.rs"],
            "created_at": "2026-04-01T00:00:00Z",
            "updated_at": "2026-04-02T00:00:00Z",
            "notes": "Added to runtime for testing.",
            "activity": "Assigned",
            "events": [
                {
                    "event_id": 42,
                    "kind": "TaskAssigned",
                    "timestamp": "2026-04-02T00:00:00Z",
                    "summary": "Task assigned to worker-1."
                }
            ]
        }),
    );
    let tool = GetTaskContextTool::new();

    let args = serde_json::json!({
        "task_id": "T-0001",
        "event_limit": 5
    });

    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());

    if let ContentBlock::Text { text } = &result.content[0] {
        let context: TaskContext = serde_json::from_str(text).unwrap();
        assert_eq!(context.task.id, "T-0001");
        assert_eq!(context.task.title, "Auth middleware");
        assert_eq!(context.dependencies, vec!["T-base-1"]);
        assert_eq!(context.related_files, vec!["src/middleware.rs"]);
        assert_eq!(context.events.len(), 1);
        assert_eq!(context.events[0].kind, "TaskAssigned");
    }
}

#[tokio::test]
async fn task_context_reads_durable_run_events_review_and_freshness() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-durable",
            "title": "Durable context",
            "description": "Read durable state",
            "status": "assigned",
            "priority": "High",
            "assignee": "worker-7",
            "dependencies": ["T-dep"],
            "updated_at": "2026-05-16T00:00:00Z"
        }),
    );
    let review_dir = brehon_root
        .join("runtime")
        .join("reviews")
        .join("T-durable");
    fs::create_dir_all(&review_dir).unwrap();
    fs::write(
        review_dir.join("state.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-durable",
            "status": "collecting",
            "current_round": 2,
            "cycle_start_round": 1,
            "current_review_id": "REV-durable",
            "max_rounds": 3,
            "panel_id": "primary",
            "panel_mode": "fixed_size",
            "panel": ["reviewer-a", "reviewer-b"],
            "submissions_received": ["reviewer-a"],
            "created_at": "2026-05-16T00:00:00Z",
            "updated_at": "2026-05-16T01:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let store = Arc::new(FjallEventStore::new(brehon_root.join("brehon.db")).unwrap());
    let event_store: Arc<dyn EventStore + Send + Sync> = store.clone();
    let run_store: Arc<dyn RunStore + Send + Sync> = store.clone();
    let now = chrono::Utc::now();
    event_store
        .append(Event {
            kind: EventKind::TaskCreated {
                task_id: "T-durable".to_string(),
            },
            timestamp: now,
            aggregate_id: "T-durable".to_string(),
        })
        .await
        .unwrap();
    event_store
        .append(Event {
            kind: EventKind::TaskAssigned {
                task_id: "T-durable".to_string(),
                agent_id: "worker-7".to_string(),
            },
            timestamp: now,
            aggregate_id: "T-durable".to_string(),
        })
        .await
        .unwrap();
    event_store
        .append(Event {
            kind: EventKind::ReviewRequested {
                task_id: "T-durable".to_string(),
                review_id: "REV-durable".to_string(),
            },
            timestamp: now,
            aggregate_id: "T-durable".to_string(),
        })
        .await
        .unwrap();

    let mut run = RunRecord::new(
        RunId::new("run-durable"),
        TaskId::new("T-durable"),
        RunRole::Worker,
        now,
    );
    run.status = RunStatus::Running;
    run.claim_generation = ClaimGeneration::new(3);
    run.claim_owner = Some(ClaimOwner::new("worker-7"));
    run.session_id = Some(SessionId::new("sess-7"));
    run_store.create_run(run).await.unwrap();

    let tool = GetTaskContextTool::new()
        .with_event_store(event_store)
        .with_run_store(run_store);
    let result = tool
        .execute(serde_json::json!({"task_id": "T-durable", "event_limit": 5}))
        .await
        .unwrap();
    let ContentBlock::Text { text } = &result.content[0] else {
        panic!("expected text content");
    };
    let context: TaskContext = serde_json::from_str(text).unwrap();
    assert_eq!(context.source_event_id, Some(3));
    assert_eq!(context.freshness.source_event_id, Some(3));
    assert!(!context.generated_at.is_empty());
    assert!(!context.truncated);
    assert!(context
        .events
        .iter()
        .any(|event| event.kind == "TaskAssigned"));
    let active_run = context.active_run.expect("active run");
    assert_eq!(active_run.run_id, "run-durable");
    assert_eq!(active_run.status, "running");
    assert_eq!(active_run.claim_generation, 3);
    assert_eq!(active_run.session_id.as_deref(), Some("sess-7"));
    let review = context.review.expect("review status");
    assert_eq!(review.review_id, "REV-durable");
    assert_eq!(review.status, "collecting");
    assert_eq!(review.panel_progress, "1/2");
}

#[tokio::test]
async fn test_get_task_context_default() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "worker-1"),
    ]);

    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-0002",
            "title": "Default task",
            "description": "The authentication middleware should validate the configuration before returning the response.",
            "status": "assigned",
            "priority": "Medium",
            "assignee": "worker-1",
            "updated_at": "2026-04-02T00:00:00Z",
        }),
    );

    let tool = GetTaskContextTool::new();

    let args = serde_json::json!({});

    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());

    if let ContentBlock::Text { text } = &result.content[0] {
        let context: TaskContext = serde_json::from_str(text).unwrap();
        assert_eq!(context.task.id, "T-0002");
        assert_eq!(context.task.status, "assigned");
        assert_eq!(context.task.assignee.as_deref(), Some("worker-1"));
        assert!(context
            .task
            .description
            .contains("authentication middleware should validate the configuration"));
    }
}

#[tokio::test]
async fn test_get_task_context_compacts_when_context_compression_enabled() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    enable_context_compression(&brehon_root);
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-0003",
            "title": "Compact task",
            "description": "The authentication middleware should validate the configuration before returning the response.",
            "status": "assigned",
            "priority": "Medium",
            "assignee": "worker-1",
            "updated_at": "2026-04-02T00:00:00Z",
            "events": [
                {
                    "event_id": 7,
                    "kind": "TaskUpdated",
                    "timestamp": "2026-04-02T00:00:00Z",
                    "summary": "The authentication middleware should validate the configuration before returning the response."
                }
            ]
        }),
    );

    let tool = GetTaskContextTool::new();
    let result = tool
        .execute(serde_json::json!({
            "task_id": "T-0003",
            "event_limit": 1
        }))
        .await
        .unwrap();
    assert!(result.is_error.is_none());

    if let ContentBlock::Text { text } = &result.content[0] {
        let context: TaskContext = serde_json::from_str(text).unwrap();
        assert!(context
            .task
            .description
            .contains("auth mw should validate config pre returning resp"));
        assert!(context.events[0]
            .summary
            .contains("auth mw should validate config pre returning resp"));
    }
}

#[test]
fn test_read_task_file_rejects_path_traversal() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    assert!(read_task_file("../../etc/passwd").is_none());
}

#[tokio::test]
async fn test_get_task_context_rejects_invalid_task_id() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    let tool = GetTaskContextTool::new();
    let result = tool
        .execute(serde_json::json!({
            "task_id": "../..//etc/passwd"
        }))
        .await;

    match result {
        Err(McpError::InvalidRequest(err)) => {
            assert!(err.contains("Invalid task_id"));
        }
        Ok(_) | Err(_) => panic!("expected invalid task id request error"),
    }
}

#[tokio::test]
async fn test_list_tasks_tool() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-0003",
            "title": "In progress item",
            "status": "in_progress",
            "priority": "high",
            "updated_at": "2026-04-03T00:00:00Z",
        }),
    );
    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-0004",
            "title": "Pending item",
            "status": "pending",
            "priority": "low",
            "updated_at": "2026-04-02T00:00:00Z",
        }),
    );

    let tool = ListTasksTool::new();

    let args = serde_json::json!({
        "status": "InProgress"
    });

    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());

    if let ContentBlock::Text { text } = &result.content[0] {
        let response: ListTasksResponse = serde_json::from_str(text).unwrap();
        assert_eq!(response.count, 1);
        assert_eq!(response.tasks[0].status, "in_progress");
    }
}

#[tokio::test]
async fn test_list_tasks_all() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_temp, brehon_root) = setup_brehon_root();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-0005",
            "title": "Task A",
            "status": "pending",
            "updated_at": "2026-04-02T00:00:00Z",
        }),
    );
    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-0006",
            "title": "Task B",
            "status": "pending",
            "updated_at": "2026-04-03T00:00:00Z",
        }),
    );

    let tool = ListTasksTool::new();

    let args = serde_json::json!({});

    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());
}

#[tokio::test]
async fn test_get_task_tool_returns_real_task_data() {
    // Regression: GetTaskTool used to return hardcoded placeholder
    // ("Implement authentication middleware") for ANY task_id, which
    // misled supervisors into thinking the task service was returning
    // wrong data while parent.children returned the real titles.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let brehon_root = root.path().join(".brehon");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    write_task(
        &brehon_root,
        &serde_json::json!({
            "task_id": "T-real-001",
            "title": "Refactor connector registration to orchestrate config/credential split",
            "description": "Split the connector registration so credentials route through SecretWriter.",
            "status": "in_progress",
            "priority": "high",
            "assignee": "kind-doe-11",
            "dependencies": ["T-dep-1", "T-dep-2"],
            "created_at": "2026-04-20T10:00:00Z",
            "updated_at": "2026-04-25T08:30:00Z",
            "events": [
                {
                    "author": "kind-doe-11",
                    "kind": "progress",
                    "summary": "Implementation 75% complete",
                    "timestamp": "2026-04-25T08:30:00Z"
                }
            ]
        }),
    );

    let tool = GetTaskTool::new();
    let result = tool
        .execute(serde_json::json!({"task_id": "T-real-001"}))
        .await
        .unwrap();
    assert!(result.is_error.is_none(), "{:?}", result);

    let ContentBlock::Text { text } = &result.content[0] else {
        panic!("expected text content");
    };
    let task: TaskDetail = serde_json::from_str(text).unwrap();
    assert_eq!(task.id, "T-real-001");
    assert_eq!(
        task.title,
        "Refactor connector registration to orchestrate config/credential split"
    );
    assert_eq!(
        task.description,
        "Split the connector registration so credentials route through SecretWriter."
    );
    assert_eq!(task.status, "in_progress");
    assert_eq!(task.priority, "high");
    assert_eq!(task.assignee.as_deref(), Some("kind-doe-11"));
    assert_eq!(task.dependencies, vec!["T-dep-1", "T-dep-2"]);
    assert_eq!(task.created_at, "2026-04-20T10:00:00Z");
    assert_eq!(task.updated_at, "2026-04-25T08:30:00Z");
    assert_eq!(task.notes.len(), 1);
    assert_eq!(task.notes[0].author, "kind-doe-11");
    assert_eq!(task.notes[0].kind, "progress");
    assert_eq!(task.notes[0].content, "Implementation 75% complete");
}

#[tokio::test]
async fn test_get_task_returns_404_for_unknown_id() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let brehon_root = root.path().join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    let tool = GetTaskTool::new();
    let result = tool
        .execute(serde_json::json!({"task_id": "T-does-not-exist"}))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let ContentBlock::Text { text } = &result.content[0] else {
        panic!("expected text content");
    };
    assert!(
        text.contains("Task not found"),
        "expected not-found message, got: {text}"
    );
}

#[tokio::test]
async fn test_get_task_rejects_invalid_id() {
    // Path-traversal / unsafe IDs must be refused, not silently
    // resolved against the filesystem.
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tool = GetTaskTool::new();
    let result = tool
        .execute(serde_json::json!({"task_id": "../../etc/passwd"}))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn test_get_task_empty_id() {
    let tool = GetTaskTool::new();

    let args = serde_json::json!({
        "task_id": ""
    });

    let result = tool.execute(args).await.unwrap();
    assert_eq!(result.is_error, Some(true));
}
