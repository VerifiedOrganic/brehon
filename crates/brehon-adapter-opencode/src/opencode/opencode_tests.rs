use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn test_server_url_from_env_wins() {
    let url = server_url_from_launch(
        &[
            "serve".to_string(),
            "--port".to_string(),
            "43100".to_string(),
        ],
        &[(
            "BREHON_OPENCODE_SERVER_URL".to_string(),
            "http://127.0.0.1:4999".to_string(),
        )],
    )
    .expect("server url");
    assert_eq!(url, "http://127.0.0.1:4999");
}

#[test]
fn test_server_url_from_args() {
    let url = server_url_from_launch(
        &[
            "serve".to_string(),
            "--port".to_string(),
            "43100".to_string(),
        ],
        &[],
    )
    .expect("server url");
    assert_eq!(url, "http://127.0.0.1:43100");
}

#[test]
fn test_server_ready_timeout_uses_env_value_or_default() {
    assert_eq!(
        server_ready_timeout_from_env_value(Some("45000")),
        Duration::from_millis(45_000)
    );
    assert_eq!(
        server_ready_timeout_from_env_value(Some(" 1200 ")),
        Duration::from_millis(1_200)
    );
    assert_eq!(
        server_ready_timeout_from_env_value(None),
        Duration::from_millis(DEFAULT_SERVER_READY_TIMEOUT_MS)
    );
    assert_eq!(
        server_ready_timeout_from_env_value(Some("0")),
        Duration::from_millis(DEFAULT_SERVER_READY_TIMEOUT_MS)
    );
    assert_eq!(
        server_ready_timeout_from_env_value(Some("invalid")),
        Duration::from_millis(DEFAULT_SERVER_READY_TIMEOUT_MS)
    );
}

#[test]
fn test_turn_start_timeout_uses_env_value_or_default() {
    assert_eq!(
        turn_start_timeout_from_env_value(Some("60000")),
        Duration::from_millis(60_000)
    );
    assert_eq!(
        turn_start_timeout_from_env_value(Some(" 1500 ")),
        Duration::from_millis(1_500)
    );
    assert_eq!(
        turn_start_timeout_from_env_value(None),
        Duration::from_millis(DEFAULT_TURN_START_TIMEOUT_MS)
    );
    assert_eq!(
        turn_start_timeout_from_env_value(Some("0")),
        Duration::from_millis(DEFAULT_TURN_START_TIMEOUT_MS)
    );
    assert_eq!(
        turn_start_timeout_from_env_value(Some("invalid")),
        Duration::from_millis(DEFAULT_TURN_START_TIMEOUT_MS)
    );
}

#[test]
fn test_should_retry_turn_error_treats_opencode_500_as_retryable() {
    assert!(should_retry_turn_error(&OpenCodeError::Http(
        "session message fetch failed with 500 Internal Server Error: \
             {\"name\":\"UnknownError\",\"data\":{\"message\":\"Unexpected server error\"}}"
            .to_string()
    )));
    assert!(should_retry_turn_error(&OpenCodeError::Turn(
        "OpenCode prompt_async failed with 500 Internal Server Error: \
             {\"name\":\"UnknownError\"}"
            .to_string()
    )));
    assert!(!should_retry_turn_error(&OpenCodeError::Turn(
        "OpenCode prompt_async failed with 400 Bad Request: invalid model".to_string()
    )));
}

#[test]
fn test_turn_poll_recovery_stops_after_budget() {
    let mut recovery = OpenCodeTurnPollRecovery::default();
    let err = OpenCodeError::Http(
        "session message fetch failed with 500 Internal Server Error".to_string(),
    );

    assert!(recovery.should_retry(&err));

    recovery.first_error_at = Some(
        tokio::time::Instant::now()
            .checked_sub(Duration::from_millis(TURN_POLL_RECOVERY_TIMEOUT_MS + 1))
            .expect("expired instant"),
    );
    assert!(!recovery.should_retry(&err));

    recovery.record_success();
    assert!(recovery.should_retry(&err));
}

#[test]
fn test_turn_progress_does_not_complete_before_activity() {
    let mut progress = OpenCodeTurnProgress::default();

    assert!(!progress.observe(false, false));
    assert!(!progress.observe(false, false));
    assert!(!progress.observe(false, false));
    assert!(!progress.saw_activity());

    assert!(!progress.observe(true, false));
    assert!(!progress.saw_activity());
    assert!(!progress.observe(false, false));
    assert!(!progress.saw_activity());
    assert!(!progress.observe(false, true));
    assert!(progress.saw_activity());
    assert!(progress.observe(false, false));
}

#[test]
fn test_turn_progress_settles_after_new_messages() {
    let mut progress = OpenCodeTurnProgress::default();

    assert!(!progress.observe(false, true));
    assert!(progress.saw_activity());
    assert!(progress.observe(false, false));
}

#[test]
fn test_set_launch_loopback_port_updates_args_and_server_url_env() {
    let mut args = vec![
        "serve".to_string(),
        "--hostname".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        "43100".to_string(),
    ];
    let mut env = vec![
        (
            "BREHON_OPENCODE_SERVER_URL".to_string(),
            "http://127.0.0.1:43100".to_string(),
        ),
        ("OTHER".to_string(), "value".to_string()),
    ];

    assert!(set_launch_loopback_port(&mut args, &mut env, 43210));
    assert_eq!(args[4], "43210");
    assert_eq!(
        server_url_from_launch(&args, &env).expect("server url"),
        "http://127.0.0.1:43210"
    );
    assert!(env
        .iter()
        .any(|(key, value)| { key == "OTHER" && value == "value" }));
}

#[test]
fn test_should_retry_spawn_with_fresh_port_only_for_port_bind_failures() {
    assert!(should_retry_spawn_with_fresh_port(&OpenCodeError::Spawn(
        "OpenCode server process exited before readiness\n\
             OpenCode process output:\n\
             opencode log /tmp/opencode.log:\n\
             Error: Failed to start server. Is port 43100 in use?"
            .to_string()
    )));
    assert!(!should_retry_spawn_with_fresh_port(&OpenCodeError::Spawn(
        "timed out waiting for OpenCode server at http://127.0.0.1:43100 after 30000ms".to_string()
    )));
    assert!(!should_retry_spawn_with_fresh_port(
        &OpenCodeError::SessionCreate("session create failed".to_string())
    ));
}

#[test]
fn test_normalize_stream_event_maps_tool_use() {
    let session_id = SessionId::new("brehon-session");
    let event: OpenCodeStreamEvent = serde_json::from_str(
        r#"{
              "type": "tool_use",
              "sessionID": "ses_1",
              "part": {
                "type": "tool",
                "tool": "bash",
                "callID": "call_123",
                "state": {
                  "status": "completed",
                  "output": "/tmp\n",
                  "title": "Prints current working directory"
                }
              }
            }"#,
    )
    .expect("parse stream event");

    let normalized = normalize_stream_event(&session_id, &event);
    assert_eq!(normalized.len(), 2);
    assert!(matches!(
        &normalized[0],
        AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "bash"
    ));
    assert!(matches!(
        &normalized[1],
        AdapterEvent::ToolCallCompleted { status, .. } if status == "completed"
    ));
}

#[test]
fn test_server_auth_from_env_extracts_credentials() {
    let auth = server_auth_from_env(&[
        ("OPENCODE_SERVER_USERNAME".to_string(), "brehon".to_string()),
        ("OPENCODE_SERVER_PASSWORD".to_string(), "secret".to_string()),
    ])
    .expect("auth");
    assert_eq!(auth.username, "brehon");
    assert_eq!(auth.password, "secret");
}

#[test]
fn test_parse_opencode_model_selection_requires_provider_model() {
    let model = parse_opencode_model_selection(" deepseek/deepseek-v4-pro[1m] ").expect("model");
    assert_eq!(model.provider_id, "deepseek");
    assert_eq!(model.model_id, "deepseek-v4-pro[1m]");

    assert!(parse_opencode_model_selection("deepseek-v4-pro").is_err());
    assert!(parse_opencode_model_selection("deepseek/model/extra").is_err());
    assert!(parse_opencode_model_selection("/deepseek-v4-pro").is_err());
}

#[test]
fn test_opencode_prompt_body_includes_model_selection() {
    let model = parse_opencode_model_selection("deepseek/deepseek-v4-pro[1m]").expect("model");
    let body = opencode_prompt_body("review this", Some(&model));

    assert_eq!(body["model"]["providerID"], "deepseek");
    assert_eq!(body["model"]["modelID"], "deepseek-v4-pro[1m]");
    assert_eq!(body["parts"][0]["type"], "text");
    assert_eq!(body["parts"][0]["text"], "review this");
}

#[test]
fn test_sse_event_surfaces_session_error() {
    let event = serde_json::json!({
        "type": "session.error",
        "properties": {
            "sessionID": "ses_123",
            "error": {
                "name": "APIError",
                "statusCode": 400,
                "message": "model rejected",
                "responseBody": "{\"error\":\"bad model\"}"
            }
        }
    });

    let normalized = normalize_open_code_server_event("ses_123", &event).expect("event");

    assert!(normalized.failure);
    assert!(normalized.message.contains("OpenCode session error"));
    assert!(normalized.message.contains("status 400"));
    assert!(normalized.message.contains("model rejected"));
    assert!(normalized.message.contains("bad model"));
}

#[test]
fn test_sse_event_name_only_error_keeps_raw_payload() {
    let event = serde_json::json!({
        "type": "session.error",
        "properties": {
            "sessionID": "ses_123",
            "error": {
                "name": "APIError"
            }
        }
    });

    let normalized = normalize_open_code_server_event("ses_123", &event).expect("event");

    assert!(normalized.failure);
    assert!(normalized.message.contains("APIError"));
    assert!(normalized
        .message
        .contains("no detail in OpenCode error payload"));
    assert!(normalized
        .message
        .contains("raw_error={\"name\":\"APIError\"}"));
}

#[test]
fn test_normalize_new_message_parts_surfaces_assistant_info_error() {
    let mut seen = OpenCodeSeenMessageParts::default();
    let message = serde_json::json!({
        "info": {
            "id": "msg-1",
            "role": "assistant",
            "error": {
                "message": "The supported API model names are deepseek-v4-pro or deepseek-v4-flash"
            }
        },
        "parts": []
    });

    let (events, tokens, saw_activity) = normalize_new_message_parts(&message, 0, &mut seen, "");

    assert_eq!(tokens, 0);
    assert!(saw_activity);
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::Output { text, .. }
            if text.contains("OpenCode assistant error")
                && text.contains("supported API model names")
    )));
}

#[test]
fn test_normalize_message_response_reads_text_parts() {
    let session_id = SessionId::new("brehon-session");
    let response = serde_json::json!({
        "parts": [
            { "type": "text", "text": "hello" },
            { "type": "text", "text": "world" }
        ]
    });
    let normalized = normalize_message_response(&session_id, &response);
    assert_eq!(normalized.len(), 2);
    assert!(matches!(
        &normalized[0],
        AdapterEvent::Output { text, .. } if text == "hello"
    ));
    assert!(matches!(
        &normalized[1],
        AdapterEvent::Output { text, .. } if text == "world"
    ));
}

#[test]
fn test_normalize_message_response_surfaces_retry_error() {
    let session_id = SessionId::new("brehon-session");
    let response = serde_json::json!({
        "role": "assistant",
        "parts": [
            {
                "type": "retry",
                "attempt": 2,
                "error": {
                    "message": "The supported API model names are deepseek-v4-pro or deepseek-v4-flash"
                }
            }
        ]
    });

    let events = normalize_message_response(&session_id, &response);

    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::Output { text, .. }
            if text.contains("OpenCode retry attempt 2 failed")
                && text.contains("supported API model names")
    )));
}

#[test]
fn test_normalize_message_response_marks_error_step_failed() {
    let session_id = SessionId::new("brehon-session");
    let response = serde_json::json!({
        "role": "assistant",
        "parts": [
            { "type": "step-finish", "reason": "error" }
        ]
    });

    let events = normalize_message_response(&session_id, &response);

    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::OperationCompleted { operation, success: false, .. }
            if operation == "step"
    )));
}

#[test]
fn test_message_tokens_used_reads_step_finish_total() {
    let response = serde_json::json!({
        "parts": [
            { "type": "text", "text": "OK" },
            {
                "type": "step-finish",
                "tokens": {
                    "total": 10058,
                    "input": 10039,
                    "output": 2,
                    "reasoning": 17
                }
            }
        ]
    });

    assert_eq!(message_tokens_used(&response), 10058);
}

#[test]
fn test_parse_session_busy_value_matches_array_entry() {
    let value: Value = serde_json::from_str(
        r#"[
              {"id":"ses_other","type":"idle"},
              {"id":"ses_123","type":"busy"}
            ]"#,
    )
    .expect("status payload");

    assert_eq!(
        parse_session_busy_value(&value, Some("ses_123")),
        Some(true)
    );
    assert_eq!(
        parse_session_busy_value(&value, Some("ses_other")),
        Some(false)
    );
}

#[test]
fn test_parse_session_busy_value_matches_object_map_entry() {
    let value: Value = serde_json::from_str(
        r#"{
              "ses_other": {"type":"idle"},
              "ses_123": {"type":"busy"}
            }"#,
    )
    .expect("status payload");

    assert_eq!(
        parse_session_busy_value(&value, Some("ses_123")),
        Some(true)
    );
    assert_eq!(
        parse_session_busy_value(&value, Some("ses_other")),
        Some(false)
    );
}

#[test]
fn test_looks_like_html_shell_detects_opencode_app_shell() {
    assert!(looks_like_html_shell(
        "<!doctype html><html><body></body></html>"
    ));
    assert!(looks_like_html_shell("<html><body></body></html>"));
    assert!(!looks_like_html_shell("{\"healthy\":true}"));
}

#[test]
fn test_normalize_message_response_prefers_assistant_messages_from_history() {
    let session_id = SessionId::new("brehon-session");
    let value: Value = serde_json::from_str(
            r#"[
              {"role":"user","parts":[{"type":"text","text":"user prompt"}]},
              {"role":"assistant","parts":[
                {"type":"step-start"},
                {"type":"tool","tool":"read","callID":"call_1","state":{"status":"completed","output":"fn main() {}\n","title":"read"}},
                {"type":"text","text":"done"}
              ]}
            ]"#,
        )
        .expect("message history");

    let events = normalize_message_response(&session_id, &value);
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::OperationStarted { operation, .. } if operation == "step"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "read"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::ToolCallCompleted { tool_name, .. } if tool_name == "read"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        AdapterEvent::Output { text, .. } if text.contains("fn main()")
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        AdapterEvent::Output { text, .. } if text.contains("user prompt")
    )));
}

#[test]
fn test_normalize_message_response_suppresses_tool_body_content() {
    let session_id = SessionId::new("brehon-session");
    let response = serde_json::json!({
        "role": "assistant",
        "parts": [
            {
                "type": "tool",
                "tool": "read",
                "callID": "call_1",
                "content": "tool body copied through content",
                "state": {
                    "status": "completed",
                    "output": "fn leaked() {}\n",
                    "title": "read"
                }
            },
            { "type": "text", "text": "done" }
        ]
    });

    let events = normalize_message_response(&session_id, &response);

    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "read"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::ToolCallCompleted { tool_name, .. } if tool_name == "read"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::Output { text, .. } if text == "done"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        AdapterEvent::Output { text, .. }
            if text.contains("fn leaked") || text.contains("tool body copied")
    )));
}

#[test]
fn test_normalize_new_message_parts_emits_only_text_delta_for_mutated_message() {
    let mut seen = OpenCodeSeenMessageParts::default();
    let first = serde_json::json!({
        "id": "msg-1",
        "role": "assistant",
        "parts": [
            {"id": "part-1", "type": "text", "text": "first"}
        ]
    });
    let second = serde_json::json!({
        "id": "msg-1",
        "role": "assistant",
        "parts": [
            {"id": "part-1", "type": "text", "text": "first second"}
        ]
    });

    let (events, tokens, saw_activity) = normalize_new_message_parts(&first, 0, &mut seen, "");
    assert_eq!(tokens, 0);
    assert!(saw_activity);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        AdapterEvent::Output { text, .. } if text == "first"
    ));

    let (events, tokens, saw_activity) = normalize_new_message_parts(&second, 0, &mut seen, "");
    assert_eq!(tokens, 0);
    assert!(saw_activity);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        AdapterEvent::Output { text, .. } if text == " second"
    ));

    let (events, tokens, saw_activity) = normalize_new_message_parts(&second, 0, &mut seen, "");
    assert_eq!(tokens, 0);
    assert!(!saw_activity);
    assert!(events.is_empty());
}

#[test]
fn test_normalize_new_message_parts_does_not_count_prompt_echo_as_activity() {
    let mut seen = OpenCodeSeenMessageParts::default();
    let prompt = "Review context\n".repeat(8);
    let echoed_prompt = serde_json::json!({
        "id": "msg-1",
        "parts": [
            {"id": "part-1", "type": "text", "text": prompt.clone()}
        ]
    });

    let (events, tokens, saw_activity) =
        normalize_new_message_parts(&echoed_prompt, 0, &mut seen, &prompt);

    assert_eq!(tokens, 0);
    assert_eq!(events.len(), 1);
    assert!(!saw_activity);
}

#[test]
fn test_normalize_new_message_parts_does_not_replay_tool_events_when_text_updates() {
    let mut seen = OpenCodeSeenMessageParts::default();
    let first = serde_json::json!({
        "id": "msg-1",
        "role": "assistant",
        "parts": [
            {"type": "step-start"},
            {
                "type": "tool",
                "tool": "bash",
                "callID": "call-1",
                "state": {
                    "status": "completed",
                    "output": "exit_code: 0\nok\n",
                    "title": "bash"
                }
            },
            {"id": "part-2", "type": "text", "text": "done"}
        ]
    });
    let second = serde_json::json!({
        "id": "msg-1",
        "role": "assistant",
        "parts": [
            {"type": "step-start"},
            {
                "type": "tool",
                "tool": "bash",
                "callID": "call-1",
                "state": {
                    "status": "completed",
                    "output": "exit_code: 0\nok\n",
                    "title": "bash"
                }
            },
            {"id": "part-2", "type": "text", "text": "done now"}
        ]
    });

    let (events, _, saw_activity) = normalize_new_message_parts(&first, 0, &mut seen, "");
    assert!(saw_activity);
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "bash"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AdapterEvent::ToolCallCompleted { tool_name, .. } if tool_name == "bash"
    )));

    let (events, _, saw_activity) = normalize_new_message_parts(&second, 0, &mut seen, "");
    assert!(saw_activity);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        AdapterEvent::Output { text, .. } if text == " now"
    ));
}

#[tokio::test]
async fn test_kill_aborts_active_prompts_and_fences_new_prompts() {
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let spec = SessionSpec::new(
        brehon_types::AgentId::new("opencode-worker"),
        "worker".to_string(),
        "/tmp".to_string(),
    );
    let inner = Arc::new(OpenCodeServerSessionInner {
        session_id,
        spec,
        process: Mutex::new(None),
        server_url: "http://127.0.0.1:0".to_string(),
        client: reqwest::Client::new(),
        auth: None,
        model: Mutex::new(None),
        opencode_session_id: Mutex::new(Some("session-1".to_string())),
        adapter_event_tx: std::sync::Mutex::new(None),
        active_prompts: Mutex::new(HashMap::new()),
        prompt_results: Arc::new(Mutex::new(HashMap::new())),
        tokens_used: AtomicU64::new(0),
        turn_lock: Mutex::new(()),
        alive: AtomicBool::new(true),
        capabilities: AgentCapabilities {
            content_block_types: vec!["text".to_string()],
            session_config_options: vec![],
            permission_support: false,
            terminal_support: false,
            tool_call_streaming: brehon_types::ToolCallStreaming::Basic,
        },
    });
    let session = OpenCodeServerSession {
        inner: inner.clone(),
        created_at: chrono::Utc::now(),
    };

    // Add a mock active prompt task
    let task_handle = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    inner
        .active_prompts
        .lock()
        .await
        .insert("prompt-1".to_string(), task_handle);

    assert!(inner.alive.load(Ordering::SeqCst));
    assert_eq!(inner.active_prompts.lock().await.len(), 1);

    session.kill().await.unwrap();

    assert!(!inner.alive.load(Ordering::SeqCst));
    assert_eq!(inner.active_prompts.lock().await.len(), 0);

    // Try to send a prompt, should fail with NotRunning
    let prompt = PromptTurn {
        prompt_id: brehon_types::PromptId::new("P-2"),
        content: "hello".to_string(),
        kind: brehon_types::MessageKind::TaskAssignment,
        sent_at: chrono::Utc::now(),
    };
    let res = session.send_prompt(prompt).await;
    assert!(matches!(res, Err(OpenCodeError::NotRunning)));
}

#[tokio::test]
async fn test_run_prompt_aborts_poll_recovery_when_server_process_missing() {
    let (server_url, server_task) = spawn_poll_failure_server().await;
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let spec = SessionSpec::new(
        brehon_types::AgentId::new("opencode-worker"),
        "worker".to_string(),
        "/tmp".to_string(),
    );
    let inner = Arc::new(OpenCodeServerSessionInner {
        session_id,
        spec,
        process: Mutex::new(None),
        server_url,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client"),
        auth: None,
        model: Mutex::new(None),
        opencode_session_id: Mutex::new(Some("session-1".to_string())),
        adapter_event_tx: std::sync::Mutex::new(None),
        active_prompts: Mutex::new(HashMap::new()),
        prompt_results: Arc::new(Mutex::new(HashMap::new())),
        tokens_used: AtomicU64::new(0),
        turn_lock: Mutex::new(()),
        alive: AtomicBool::new(true),
        capabilities: AgentCapabilities {
            content_block_types: vec!["text".to_string()],
            session_config_options: vec![],
            permission_support: false,
            terminal_support: false,
            tool_call_streaming: brehon_types::ToolCallStreaming::Basic,
        },
    });

    let prompt = PromptTurn {
        prompt_id: brehon_types::PromptId::new("P-1"),
        content: "hello".to_string(),
        kind: brehon_types::MessageKind::TaskAssignment,
        sent_at: chrono::Utc::now(),
    };

    let err = run_prompt(inner, prompt).await.unwrap_err();
    assert!(matches!(
        err,
        OpenCodeError::Http(message)
            if message.contains("OpenCode server process exited mid-turn")
                && message.contains("session message fetch failed with 500")
    ));
    server_task.abort();
}

async fn spawn_poll_failure_server() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let message_calls = Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let message_calls = message_calls.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let Ok(bytes_read) = socket.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..bytes_read]);
                let first_line = request.lines().next().unwrap_or_default();
                let (status, body) = if first_line.starts_with("GET /session/session-1/message") {
                    let call = message_calls.fetch_add(1, Ordering::SeqCst);
                    if call == 0 {
                        ("200 OK", "[]")
                    } else {
                        (
                            "500 Internal Server Error",
                            r#"{"name":"UnknownError","data":{"message":"Unexpected server error"}}"#,
                        )
                    }
                } else if first_line.starts_with("POST /session/session-1/prompt_async") {
                    ("200 OK", "{}")
                } else {
                    ("404 Not Found", "{}")
                };
                let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    (format!("http://{addr}"), handle)
}
