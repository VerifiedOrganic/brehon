use super::*;

#[tokio::test]
async fn test_unblock_returns_external_blocked_task_to_pending_frontier() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-sdk-pin", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-sdk-pin");
    task["assignee"] = Value::String("worker-1".to_string());
    task["percent"] = Value::Number(serde_json::Number::from(20_u64));
    task["latest_commit"] = Value::String("29ffd473bc583ab12ceb605f5fe9203bb6fe4636".to_string());
    task["activity"] = Value::String("waiting_on_supervisor".to_string());
    task["inbox_delivered"] = Value::Bool(true);
    task["assignment_propagation"] = serde_json::json!({
        "owner": "worker-1",
        "prompt_id": "prompt-1",
        "delivery_method": "direct"
    });
    task["blockers"] = Value::String(
        "Blocked until the SDK pin includes NetconfSshSmokeClientConfig and run_netconf_ssh_smoke."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-sdk-pin.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "unblock",
            "id": "T-sdk-pin",
            "reason": "SDK pin updated to c4b6f2f9 with the NETCONF SSH smoke client available."
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["action"], "unblock");
    assert_eq!(payload["from_status"], "blocked");
    assert_eq!(payload["to_status"], "pending");
    assert_eq!(payload["cleared_assignee"], "worker-1");
    assert_eq!(payload["dependencies_blocking"], serde_json::json!([]));
    assert_eq!(payload["next_action"]["kind"], "refresh_frontier");

    let after = read_test_task(root.path(), "T-sdk-pin");
    assert_eq!(after["status"], "pending");
    assert_eq!(after["assignee"], Value::Null);
    assert_eq!(after["review_owner"], Value::Null);
    assert_eq!(after["inbox_delivered"], false);
    assert_eq!(
        after["latest_commit"],
        "29ffd473bc583ab12ceb605f5fe9203bb6fe4636"
    );
    assert_eq!(after["percent"], 20);
    assert!(after.get("blockers").is_none());
    assert!(after.get("activity").is_none());
    assert!(after.get("assignment_propagation").is_none());
    assert!(after["unblock_note"]
        .as_str()
        .unwrap()
        .contains("SDK pin updated"));
}

#[tokio::test]
async fn test_unblock_rejects_slow_active_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-slow", "in_progress", "task");
    let result = tool
        .execute(serde_json::json!({
            "action": "unblock",
            "id": "T-slow",
            "reason": "worker appears slow"
        }))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["error_code"], "unblock_wrong_status");

    let after = read_test_task(root.path(), "T-slow");
    assert_eq!(after["status"], "in_progress");
    assert_eq!(after["assignee"], "worker-1");
}

#[tokio::test]
async fn test_unblock_requires_reason_and_blocking_evidence() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-no-reason", "blocked", "task");
    let missing_reason = tool
        .execute(serde_json::json!({
            "action": "unblock",
            "id": "T-no-reason"
        }))
        .await
        .unwrap();
    assert_eq!(missing_reason.is_error, Some(true));
    let payload: Value = serde_json::from_str(&extract_text(&missing_reason)).unwrap();
    assert_eq!(payload["error_code"], "missing_unblock_reason");

    let missing_evidence = tool
        .execute(serde_json::json!({
            "action": "unblock",
            "id": "T-no-reason",
            "reason": "external blocker resolved"
        }))
        .await
        .unwrap();
    assert_eq!(missing_evidence.is_error, Some(true));
    let payload: Value = serde_json::from_str(&extract_text(&missing_evidence)).unwrap();
    assert_eq!(payload["error_code"], "unblock_missing_blocking_evidence");

    let after = read_test_task(root.path(), "T-no-reason");
    assert_eq!(after["status"], "blocked");
}

#[tokio::test]
async fn test_unblock_does_not_bypass_unmet_dependencies() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-dep", "pending", "task");
    write_test_task(root.path(), "T-dependent", "blocked", "task");
    let mut dependent = read_test_task(root.path(), "T-dependent");
    dependent["blockers"] = Value::String("Waiting for dependency T-dep.".to_string());
    dependent["dependencies"] = serde_json::json!(["T-dep"]);
    dependent["blocked_by"] = serde_json::json!(["T-dep"]);
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
            "action": "unblock",
            "id": "T-dependent",
            "reason": "manual blocker text is stale; dependency reconciliation should decide readiness"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["to_status"], "blocked");
    assert_eq!(
        payload["dependencies_blocking"],
        serde_json::json!(["T-dep"])
    );

    let after = read_test_task(root.path(), "T-dependent");
    assert_eq!(after["status"], "blocked");
    assert_eq!(after["blocked_by"], serde_json::json!(["T-dep"]));
    assert!(after.get("blockers").is_none());
}

#[tokio::test]
async fn test_recover_handoff_action_repairs_empty_assignee_rather_than_blocker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-empty-assignee", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-empty-assignee");
    task["latest_commit"] = Value::String("7de5f572777af2233d3c78c8d823495dc64b4e2e".to_string());
    task["assignee"] = Value::String("worker-1".to_string());
    task["blockers"] = Value::String(
        "Brehon task state changed to pending/unassigned while work was in progress. \
         The triage report is staged, but task action=complete is rejected because \
         T-empty-assignee is assigned to '' rather than worker-1."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-empty-assignee.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-empty-assignee"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["action"], "recover_handoff");
    assert_eq!(payload["from_status"], "blocked");
    assert_eq!(payload["to_status"], "review_ready");
    assert_eq!(
        payload["latest_commit"],
        "7de5f572777af2233d3c78c8d823495dc64b4e2e"
    );

    let after = read_test_task(root.path(), "T-empty-assignee");
    assert_eq!(after["status"], "review_ready");
    assert_eq!(after["assignee"], Value::Null);
    assert!(after.get("blockers").is_none());
}

#[tokio::test]
async fn test_recover_handoff_action_repairs_environment_limited_checkpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-env-checkpoint", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-env-checkpoint");
    task["latest_commit"] = Value::String("510e1bd4dc454a8dab7b82ffb3f8f3c9f6687851".to_string());
    task["assignee"] = Value::String("worker-1".to_string());
    task["notes"] = Value::String(
        "Validation pass found and fixed a RustSec advisory by updating Cargo.lock. \
         Remaining final validation cannot be completed in this pane because AF_UNIX socket \
         creation is denied and Go 1.26.4 is unavailable under network-restricted GOTOOLCHAIN."
            .to_string(),
    );
    task["blockers"] = Value::String(
        "Final validation cannot be completed in this worker pane after checkpoint 510e1bd. \
         Local environment blockers: AF_UNIX socket creation is denied by sandbox with \
         Operation not permitted; Go toolchain download is blocked by network/DNS; \
         cargo deny advisories cannot lock the advisory database. Completed checkpoint includes \
         operator-readiness docs update and RustSec lockfile fix."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-env-checkpoint.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-env-checkpoint"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["action"], "recover_handoff");
    assert_eq!(payload["from_status"], "blocked");
    assert_eq!(payload["to_status"], "review_ready");
    assert_eq!(
        payload["latest_commit"],
        "510e1bd4dc454a8dab7b82ffb3f8f3c9f6687851"
    );

    let after = read_test_task(root.path(), "T-env-checkpoint");
    assert_eq!(after["status"], "review_ready");
    assert_eq!(after["assignee"], Value::Null);
    assert_eq!(after["activity"], "awaiting_review");
    assert!(after.get("blockers").is_none());
    assert!(after["recovery_note"]
        .as_str()
        .unwrap()
        .contains("environment-limited checkpoint"));
}

#[tokio::test]
async fn test_recover_handoff_action_repairs_fresh_post_review_checkpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-review-checkpoint", "blocked", "task");
    write_review_metadata_with_commits(
        root.path(),
        "T-review-checkpoint",
        "changes_requested",
        "1111111111111111111111111111111111111111",
        &["1111111111111111111111111111111111111111"],
    );
    let mut task = read_test_task(root.path(), "T-review-checkpoint");
    task["latest_commit"] = Value::String("2222222222222222222222222222222222222222".to_string());
    task["review_feedback"] = serde_json::json!({
        "outcome": "changes_requested",
        "review_id": "REV-test",
        "round": 1
    });
    task["blockers"] = Value::String(
        "Round 1 followups have been addressed and checkpointed in \
         2222222222222222222222222222222222222222; remaining blocker is local tool availability."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-review-checkpoint.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-review-checkpoint"
        }))
        .await
        .unwrap();

    assert!(result.is_error.is_none(), "{}", extract_text(&result));
    let payload: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(payload["action"], "recover_handoff");
    assert_eq!(payload["from_status"], "blocked");
    assert_eq!(payload["to_status"], "review_ready");
    assert_eq!(
        payload["latest_commit"],
        "2222222222222222222222222222222222222222"
    );
    assert_eq!(payload["next_action"]["kind"], "request_review");

    let after = read_test_task(root.path(), "T-review-checkpoint");
    assert_eq!(after["status"], "review_ready");
    assert_eq!(after["percent"], 100);
    assert_eq!(after["activity"], "awaiting_review");
    assert!(after.get("blockers").is_none());
    assert_eq!(after["assignee"], Value::Null);
    assert_eq!(after["review_owner"], Value::Null);
    assert!(after["recovery_note"]
        .as_str()
        .unwrap()
        .contains("post-review checkpoint"));
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
async fn test_ready_next_action_force_reassigns_stalled_changes_requested_task() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_STALLED_CHANGES_REQUESTED_SECS", "1"),
    ]);
    let tool = TaskActionsTool::new();

    write_test_task(
        root.path(),
        "T-stalled-revision",
        "changes_requested",
        "task",
    );
    let mut task = read_test_task(root.path(), "T-stalled-revision");
    task["updated_at"] =
        Value::String((chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339());
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-stalled-revision.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();

    assert_eq!(payload["changes_requested_count"], 0, "{payload}");
    assert_eq!(payload["stalled_count"], 1, "{payload}");
    assert_eq!(payload["stalled_tasks"][0]["task_id"], "T-stalled-revision");
    assert_eq!(
        payload["next_action"]["kind"],
        "force_reassign_stalled_revision_worker"
    );
    assert_eq!(payload["next_action"]["tool"], "factory");
    assert_eq!(payload["next_action"]["args"]["action"], "assign_workers");
    assert_eq!(
        payload["next_action"]["args"]["task_id"],
        "T-stalled-revision"
    );
    assert_eq!(payload["next_action"]["args"]["force_reassign"], true);
    assert_eq!(payload["next_action"]["requires"][0], "workers");
}

#[tokio::test]
async fn test_ready_surfaces_empty_assignee_rather_than_worker_handoff() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-empty-assignee", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-empty-assignee");
    task["latest_commit"] = Value::String("7de5f572777af2233d3c78c8d823495dc64b4e2e".to_string());
    task["assignee"] = Value::String("worker-1".to_string());
    task["blockers"] = Value::String(
        "Brehon task state changed to pending/unassigned while work was in progress. \
         The triage report is staged, but task action=complete is rejected because \
         T-empty-assignee is assigned to '' rather than worker-1."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-empty-assignee.json"),
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
        "T-empty-assignee"
    );
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["recovery_kind"],
        "worker_handoff"
    );
    assert_eq!(payload["blocked_handoff_tasks"][0]["safe_repair"], true);
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["action"],
        "recover_handoff"
    );
    assert_eq!(payload["next_action"]["kind"], "repair_frontier");
}

#[tokio::test]
async fn test_ready_surfaces_environment_limited_checkpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-env-checkpoint", "blocked", "task");
    let mut task = read_test_task(root.path(), "T-env-checkpoint");
    task["latest_commit"] = Value::String("510e1bd4dc454a8dab7b82ffb3f8f3c9f6687851".to_string());
    task["assignee"] = Value::String("worker-1".to_string());
    task["notes"] = Value::String(
        "Validation pass found and fixed a RustSec advisory by updating Cargo.lock. \
         Remaining final validation cannot be completed in this pane because AF_UNIX socket \
         creation is denied and Go 1.26.4 is unavailable under network-restricted GOTOOLCHAIN."
            .to_string(),
    );
    task["blockers"] = Value::String(
        "Final validation cannot be completed in this worker pane after checkpoint 510e1bd. \
         Local environment blockers: AF_UNIX socket creation is denied by sandbox with \
         Operation not permitted; Go toolchain download is blocked by network/DNS; \
         cargo deny advisories cannot lock the advisory database. Completed checkpoint includes \
         operator-readiness docs update and RustSec lockfile fix."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-env-checkpoint.json"),
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
        "T-env-checkpoint"
    );
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["recovery_kind"],
        "environment_limited_checkpoint"
    );
    assert_eq!(payload["blocked_handoff_tasks"][0]["safe_repair"], true);
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["action"],
        "recover_handoff"
    );
    assert_eq!(payload["next_action"]["kind"], "repair_frontier");
}

#[tokio::test]
async fn test_ready_surfaces_recoverable_blocked_post_review_checkpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-review-checkpoint", "blocked", "task");
    write_review_metadata_with_commits(
        root.path(),
        "T-review-checkpoint",
        "changes_requested",
        "1111111111111111111111111111111111111111",
        &["1111111111111111111111111111111111111111"],
    );
    let mut task = read_test_task(root.path(), "T-review-checkpoint");
    task["latest_commit"] = Value::String("2222222222222222222222222222222222222222".to_string());
    task["assignee"] = Value::Null;
    task["review_feedback"] = serde_json::json!({
        "outcome": "changes_requested",
        "review_id": "REV-test",
        "round": 1
    });
    task["blockers"] = Value::String(
        "Round 1 code/doc followups have been addressed and checkpointed in \
         2222222222222222222222222222222222222222; remaining blocker is the local cargo-fuzz gate."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-review-checkpoint.json"),
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
        "T-review-checkpoint"
    );
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["recovery_kind"],
        "review_checkpoint"
    );
    assert_eq!(payload["blocked_handoff_count"], 1, "{payload}");
    assert_eq!(payload["blocked_handoff_tasks"][0]["safe_repair"], true);
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["action"],
        "recover_handoff"
    );
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["id"],
        "T-review-checkpoint"
    );
    assert_eq!(payload["next_action"]["kind"], "repair_frontier");
    assert!(payload["next_action"]["description"]
        .as_str()
        .unwrap()
        .contains("post-review checkpoint"));
}

#[tokio::test]
async fn test_ready_surfaces_recoverable_blocked_checkpoint_with_env_limited_validation() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_test_task(root.path(), "T-env-validation", "blocked", "task");
    write_review_metadata_with_commits(
        root.path(),
        "T-env-validation",
        "changes_requested",
        "1111111111111111111111111111111111111111",
        &["1111111111111111111111111111111111111111"],
    );
    let mut task = read_test_task(root.path(), "T-env-validation");
    task["latest_commit"] = Value::String("3ecc32a5427a4e4354ccc246d2847f7a9c5840ea".to_string());
    task["assignee"] = Value::String("worker-1".to_string());
    task["review_feedback"] = serde_json::json!({
        "outcome": "changes_requested",
        "review_id": "REV-test",
        "round": 1
    });
    task["blockers"] = Value::String(
        "Checkpoint fix is 3ecc32a5427a4e4354ccc246d2847f7a9c5840ea. \
         Phase validation is blocked by sandbox/environment limits: AF_UNIX socket bind \
         in /tmp returns PermissionDenied/Operation not permitted for FakeKms/opc-security-testkit."
            .to_string(),
    );
    std::fs::write(
        root.path()
            .join("runtime")
            .join("tasks")
            .join("T-env-validation.json"),
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
        "T-env-validation"
    );
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["recovery_kind"],
        "review_checkpoint"
    );
    assert_eq!(payload["blocked_handoff_tasks"][0]["safe_repair"], true);
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["action"],
        "recover_handoff"
    );
    assert_eq!(payload["next_action"]["kind"], "repair_frontier");
}
