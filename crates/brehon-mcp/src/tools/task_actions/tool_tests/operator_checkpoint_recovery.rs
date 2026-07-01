use super::*;

fn write_operator_directed_checkpoint_task(root: &Path) {
    write_test_task(root, "T-superseded", "blocked", "task");
    let mut task = read_test_task(root, "T-superseded");
    task["latest_commit"] = Value::String("29ffd473bc583ab12ceb605f5fe9203bb6fe4636".to_string());
    task["assignee"] = Value::Null;
    task["blockers"] = Value::String(
        "Superseded husk pending checkpoint-review discharge. Reset to blocked to \
         re-enable recover_handoff so stale checkpoint 29ffd473 can enter review_ready \
         and be adjudicated/archived (work superseded by in-flight T-b02be285)."
            .to_string(),
    );
    std::fs::write(
        root.join("runtime").join("tasks").join("T-superseded.json"),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn test_ready_surfaces_operator_directed_checkpoint_recovery() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[("BREHON_ROOT", root.path().to_str().unwrap())]);
    let tool = TaskActionsTool::new();

    write_operator_directed_checkpoint_task(root.path());

    let ready = tool
        .execute(serde_json::json!({"action": "ready"}))
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&extract_text(&ready)).unwrap();
    assert_eq!(payload["recoverable_blocked_count"], 1, "{payload}");
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["task_id"],
        "T-superseded"
    );
    assert_eq!(
        payload["recoverable_blocked_tasks"][0]["recovery_kind"],
        "operator_checkpoint_recovery"
    );
    assert_eq!(payload["blocked_handoff_count"], 1, "{payload}");
    assert_eq!(payload["blocked_handoff_tasks"][0]["safe_repair"], true);
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["action"],
        "recover_handoff"
    );
    assert_eq!(
        payload["blocked_handoff_tasks"][0]["repair_action"]["args"]["id"],
        "T-superseded"
    );
    assert_eq!(payload["next_action"]["kind"], "repair_frontier");
}

#[tokio::test]
async fn test_recover_handoff_action_repairs_operator_directed_checkpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = make_test_root();
    let _env = ScopedEnv::set_with_defaults(&[
        ("BREHON_ROOT", root.path().to_str().unwrap()),
        ("BREHON_AGENT_ROLE", "supervisor"),
    ]);
    let tool = TaskActionsTool::new();

    write_operator_directed_checkpoint_task(root.path());

    let result = tool
        .execute(serde_json::json!({
            "action": "recover_handoff",
            "id": "T-superseded"
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
        "29ffd473bc583ab12ceb605f5fe9203bb6fe4636"
    );
    assert_eq!(payload["next_action"]["kind"], "request_review");

    let after = read_test_task(root.path(), "T-superseded");
    assert_eq!(after["status"], "review_ready");
    assert_eq!(after["percent"], 100);
    assert_eq!(after["activity"], "awaiting_review");
    assert!(after.get("blockers").is_none());
    assert!(after["recovery_note"]
        .as_str()
        .unwrap()
        .contains("operator-directed checkpoint"));
}
