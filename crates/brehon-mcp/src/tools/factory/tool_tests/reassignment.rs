use super::*;

#[tokio::test]
async fn test_assign_workers_force_reassigns_live_changes_requested_transfer() {
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

    let rejected = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-live-revision",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    let task = read_test_task(root.path(), "T-live-revision");
    assert_eq!(rejected.is_error, Some(true));
    let text = extract_text(&rejected);
    assert!(text.contains("without force_reassign=true"), "{text}");
    assert_eq!(task["status"], "changes_requested");
    assert_eq!(task["assignee"], "worker-1");

    let accepted = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-live-revision",
            "worker": "worker-2",
            "force_reassign": true
        }))
        .await
        .unwrap();

    assert!(accepted.is_error.is_none(), "{}", extract_text(&accepted));
    let payload: Value = serde_json::from_str(&extract_text(&accepted)).unwrap();
    assert_eq!(payload["force_reassigned"], true);
    assert_eq!(payload["reassigned_from"], "worker-1");
    assert_eq!(payload["previous_worker_recycle_target"], "worker-1");
    assert_eq!(payload["previous_worker_recycle_queued"], true);

    let task = read_test_task(root.path(), "T-live-revision");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-2");
    assert!(task["reassignment_note"]
        .as_str()
        .unwrap_or("")
        .contains("worker-1"));
}

#[tokio::test]
async fn test_assign_workers_force_reassigns_live_in_progress_task() {
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

    let rejected = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-live-progress",
            "worker": "worker-2"
        }))
        .await
        .unwrap();

    assert_eq!(rejected.is_error, Some(true));
    assert!(
        extract_text(&rejected).contains("without force_reassign=true"),
        "{}",
        extract_text(&rejected)
    );

    let task = read_test_task(root.path(), "T-live-progress");
    assert_eq!(task["status"], "in_progress");
    assert_eq!(task["assignee"], "worker-1");

    let accepted = tool
        .execute(serde_json::json!({
            "action": "assign_workers",
            "task_id": "T-live-progress",
            "worker": "worker-2",
            "force_reassign": true
        }))
        .await
        .unwrap();

    assert!(accepted.is_error.is_none(), "{}", extract_text(&accepted));
    let payload: Value = serde_json::from_str(&extract_text(&accepted)).unwrap();
    assert_eq!(payload["force_reassigned"], true);
    assert_eq!(payload["reassigned_from"], "worker-1");
    assert_eq!(payload["previous_worker_recycle_target"], "worker-1");

    let task = read_test_task(root.path(), "T-live-progress");
    assert_eq!(task["status"], "assigned");
    assert_eq!(task["assignee"], "worker-2");
}
