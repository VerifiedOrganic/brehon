use super::*;

#[tokio::test]
async fn test_ready_surfaces_integrated_aborted_closeout_tasks() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-closeout", "approved", "task");
    let mut task = read_test_task(root.path(), "T-closeout");
    task["completion_mode"] = Value::String("merge".to_string());
    task["merge_target"] = Value::String("epic/test".to_string());
    task["integration_status"] = Value::String("integrated".to_string());
    task["integration"] = serde_json::json!({
        "phase": "aborted",
        "epic_branch": "epic/test",
        "reviewed_commits": ["abc123"],
        "resolution": {
            "kind": "manual_abort",
            "reason": "operator stopped after commit landed",
            "resolved_at": "2026-04-23T00:01:00Z"
        }
    });
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-closeout.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["integrated_closeout_count"], 1, "{payload}");
    assert_eq!(
        payload["integrated_closeout_tasks"][0]["task_id"],
        "T-closeout"
    );
    assert_eq!(
        payload["integrated_closeout_tasks"][0]["integration_phase"],
        "aborted"
    );
    assert_eq!(payload["approved_count"], 0, "{payload}");
    assert_eq!(payload["next_action"]["kind"], "integrate_closeout");
    assert_eq!(payload["next_action"]["args"]["action"], "integrate");
    assert_eq!(payload["next_action"]["args"]["id"], "T-closeout");
}

#[tokio::test]
async fn test_integrate_action_finalizes_aborted_integrated_task_when_latest_commit_is_present() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-closeout"],
    );
    std::fs::write(workspace.path().join("closeout.txt"), "already landed\n").unwrap();
    run_git(workspace.path(), &["add", "closeout.txt"]);
    run_git(workspace.path(), &["commit", "-m", "closeout work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);
    run_git(workspace.path(), &["checkout", "-b", "epic/test-feature"]);
    run_git(workspace.path(), &["cherry-pick", "-x", &reviewed_commit]);
    let epic_head = run_git(workspace.path(), &["rev-parse", "HEAD"]);
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
    let subtask_json = create_subtask_for_test(&tool, "Closeout Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "integrated".into();
    task["latest_commit"] = reviewed_commit.clone().into();
    task["integration"] = serde_json::json!({
        "phase": "aborted",
        "epic_branch": "epic/test-feature",
        "worktree_path": "",
        "reviewed_commits": [reviewed_commit],
        "started_at": "2026-04-23T00:00:00Z",
        "last_transition_at": "2026-04-23T00:01:00Z",
        "conflicting_files": [],
        "attempts": 1,
        "resolution": {
            "kind": "manual_abort",
            "reason": "operator stopped after commit landed",
            "resolved_at": "2026-04-23T00:01:00Z"
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

    write_test_task(&brehon_root, "T-dependent-closeout", "blocked", "task");
    let mut dependent = read_test_task(&brehon_root, "T-dependent-closeout");
    dependent["dependencies"] = serde_json::json!([subtask_id]);
    dependent["blocked_by"] = serde_json::json!([subtask_id]);
    dependent["assignee"] = Value::Null;
    std::fs::write(
        brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-dependent-closeout.json"),
        serde_json::to_string_pretty(&dependent).unwrap(),
    )
    .unwrap();

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
    assert_eq!(result["terminal_status"], "closed");
    assert_eq!(result["already_integrated"], Value::Bool(true));
    assert_eq!(result["merged_commit"], epic_head);

    let integration_worktree = result["integration_worktree"].as_str().unwrap();
    assert_eq!(
        run_git(Path::new(integration_worktree), &["rev-parse", "HEAD"]),
        epic_head,
        "finalize-only closeout must not create a new cherry-pick commit"
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "complete");
    assert_eq!(stored["merged_branch"], "epic/test-feature");
    assert_eq!(stored["merged_commit"], epic_head);
    assert!(
        stored["integration"]["resolution"].is_null(),
        "final closeout should clear the stale aborted resolution"
    );

    let dependent_after = read_test_task(&brehon_root, "T-dependent-closeout");
    assert_eq!(dependent_after["status"], "pending");
    assert!(
        dependent_after.get("blocked_by").is_none()
            || dependent_after["blocked_by"] == serde_json::json!([]),
        "dependency should be unblocked after closeout: {dependent_after:?}"
    );
}

#[tokio::test]
async fn test_integrate_action_rejects_aborted_integrated_task_when_commit_is_missing() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = make_test_root();
    let brehon_root = workspace.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).unwrap();

    init_git_workspace(workspace.path());
    run_git(
        workspace.path(),
        &["checkout", "-b", "worker/task-missing-closeout"],
    );
    std::fs::write(workspace.path().join("missing.txt"), "not landed\n").unwrap();
    run_git(workspace.path(), &["add", "missing.txt"]);
    run_git(workspace.path(), &["commit", "-m", "missing closeout work"]);
    let reviewed_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);
    run_git(workspace.path(), &["checkout", "main"]);
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
    let subtask_json = create_subtask_for_test(&tool, "Missing Closeout Subtask", epic_id).await;
    let subtask_id = subtask_json["task_id"].as_str().unwrap();

    let mut task = read_test_task(&brehon_root, subtask_id);
    task["status"] = "approved".into();
    task["integration_status"] = "integrated".into();
    task["latest_commit"] = reviewed_commit.clone().into();
    task["integration"] = serde_json::json!({
        "phase": "aborted",
        "epic_branch": "epic/test-feature",
        "reviewed_commits": [reviewed_commit],
        "resolution": {
            "kind": "manual_abort",
            "reason": "operator stopped before commit landed",
            "resolved_at": "2026-04-23T00:01:00Z"
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

    let integrate_result = tool
        .execute(serde_json::json!({
            "action": "integrate",
            "id": subtask_id
        }))
        .await
        .unwrap();

    assert_eq!(integrate_result.is_error, Some(true));
    let result: Value = serde_json::from_str(&extract_text(&integrate_result)).unwrap();
    assert_eq!(result["error_code"], "integration_aborted");
    assert_eq!(result["integration_phase"], "aborted");
    assert_eq!(result["next_action_for_supervisor"]["kind"], "force_retry");
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("reviewed commit set is not present on merge target"),
        "{result}"
    );

    let stored = read_test_task(&brehon_root, subtask_id);
    assert_eq!(stored["status"], "approved");
    assert_eq!(stored["integration_status"], "integrated");
    assert_eq!(stored["integration"]["phase"], "aborted");
    assert!(stored.get("closed_at").is_none());
}
