use super::*;
use crate::tools::TEST_ENV_LOCK;
use async_trait::async_trait;
use std::ffi::OsString;
use std::fs;

struct ScopedEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut all_vars: Vec<(&'static str, &str)> = vars.to_vec();
        let auto_clear = [
            "BREHON_ROOT",
            "BREHON_PROJECT_ROOT",
            "BREHON_SESSION_NAME",
            "BREHON_WORKSPACE_ROOT",
            "BREHON_WORKTREE_BRANCH",
        ];
        for key in auto_clear {
            if !all_vars.iter().any(|(existing, _)| *existing == key) {
                all_vars.push((key, ""));
            }
        }

        let mut saved = Vec::with_capacity(all_vars.len());
        for (key, value) in all_vars {
            saved.push((key, std::env::var_os(key)));
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

struct PanicTool;

#[async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "panic_tool"
    }

    fn description(&self) -> &str {
        "Panics to verify MCP panic boundaries."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult, McpError> {
        panic!("boom")
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo_tool"
    }

    fn description(&self) -> &str {
        "Echoes caller payload for request-size guard testing."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "payload": { "type": "string" }
            },
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult, McpError> {
        Ok(ToolResult {
            content: vec![ContentBlock::Text {
                text: "ok".to_string(),
            }],
            is_error: None,
        })
    }
}

struct SmallBoundTool;

#[async_trait]
impl Tool for SmallBoundTool {
    fn name(&self) -> &str {
        "small_bound_tool"
    }

    fn description(&self) -> &str {
        "Uses a tighter per-tool size bound."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    fn max_argument_bytes(&self) -> Option<usize> {
        Some(32)
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult, McpError> {
        Ok(ToolResult {
            content: vec![ContentBlock::Text { text: "ok".into() }],
            is_error: None,
        })
    }
}

fn error_payload(message: &str) -> Value {
    serde_json::from_str(message).expect("structured tool error should be json")
}

#[test]
fn test_register_builtin_tools() {
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_builtin_tools();
    assert!(server.tools.len() >= 10);
    assert!(server.tools.contains_key("agent"));
    assert!(server.tools.contains_key("task"));
    assert!(server.tools.contains_key("search_memories"));
}

#[tokio::test]
async fn research_role_lists_only_read_only_context_and_research_tools() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[
        ("BREHON_SESSION_ID", "sess-research"),
        ("BREHON_AGENT_NAME", "research-1"),
        ("BREHON_AGENT_ROLE", "research"),
    ]);
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_builtin_tools();

    let tool_names: Vec<String> = server
        .tool_definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    for allowed in [
        "health",
        "search_memories",
        "get_memories",
        "list_memories",
        "search_rules",
        "search_skills",
        "get_task_context",
        "list_tasks",
        "get_task",
        "agent",
        "research",
    ] {
        assert!(
            tool_names.iter().any(|name| name == allowed),
            "research role should list {allowed}; got {tool_names:?}"
        );
    }

    for blocked in [
        "task",
        "factory",
        "verification",
        "advisor",
        "create_memory",
        "create_rule",
        "delete_memory",
    ] {
        assert!(
            !tool_names.iter().any(|name| name == blocked),
            "research role should not list {blocked}; got {tool_names:?}"
        );
    }

    let result = server
        .call_tool("task", serde_json::json!({ "action": "list" }))
        .await;
    match result {
        Err(McpError::InvalidRequest(message)) => {
            let payload = error_payload(&message);
            assert_eq!(payload["error_code"], "mcp_tool_forbidden_for_role");
            assert_eq!(payload["error"]["tool"], "task");
            assert_eq!(payload["error"]["caller"]["role"], "research");
            assert!(payload["error"]["fields"]["allowed_tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool == "research"));
        }
        other => panic!("expected research role tool rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn test_brehon_service_list_tools() {
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_builtin_tools();

    let service = BrehonService::new(server);
    let info = service.get_info();
    assert_eq!(info.server_info.name, "test-server");
}

#[tokio::test]
async fn test_brehon_service_agent_role_uses_session_start_bootstrap() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[
        ("BREHON_AGENT_ROLE", "worker"),
        ("BREHON_AGENT_TYPE", "agy"),
        ("BREHON_AGENT_NAME", "agy-worker-1"),
        ("BREHON_SUPERVISOR_NAME", "claude-supervisor"),
    ]);
    let server = McpServer::new("test-server", "1.0.0");

    let service = BrehonService::new(server);
    let info = service.get_info();
    let instructions = info.instructions.expect("bootstrap instructions");

    assert!(instructions.contains("immediately call the `agent` tool"));
    assert!(instructions.contains("`action=session_start`"));
}

#[tokio::test]
async fn test_brehon_service_without_agent_role_has_no_bootstrap_instructions() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_AGENT_ROLE", ""), ("BREHON_AGENT_TYPE", "codex")]);
    let server = McpServer::new("test-server", "1.0.0");

    let service = BrehonService::new(server);
    let info = service.get_info();
    assert!(info.instructions.is_none());
}

#[tokio::test]
async fn test_call_tool_via_registry() {
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_builtin_tools();

    let tool = server.tools.get("agent").expect("agent tool registered");
    let args = serde_json::json!({ "action": "whoami" });
    let result = tool.execute(args).await.unwrap();
    assert!(result.is_error.is_none());
}

#[test]
fn test_tool_not_found() {
    let server = McpServer::new("test-server", "1.0.0");
    assert!(!server.tools.contains_key("nonexistent"));
}

#[test]
fn test_call_tool_rejected_when_draining() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    brehon_types::drain::reset_draining_for_test();
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_builtin_tools();

    // Set draining before the call
    brehon_types::drain::set_draining();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.call_tool("agent", serde_json::json!({ "action": "whoami" })));
    assert!(result.is_err(), "call_tool should reject when draining");

    brehon_types::drain::reset_draining_for_test();
}

#[test]
fn test_load_project_review_config_uses_brehon_root_parent() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let brehon_dir = temp.path().join(".brehon");
    fs::create_dir_all(&brehon_dir).unwrap();
    let mut config = brehon_config::parse_defaults().unwrap();
    config.review.panel_mode = brehon_types::config::ReviewPanelMode::FixedSize;
    config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
    config.review.panels = vec![brehon_types::config::ReviewPanelConfig {
        id: "primary".to_string(),
        reviewers: vec!["codex-reviewer".to_string(), "gemini-reviewer".to_string()],
    }];
    fs::write(
        brehon_dir.join("config.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_dir.to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", ""),
    ]);
    std::env::remove_var("BREHON_PROJECT_ROOT");

    // Assertions chosen to differ from defaults so the project layer is
    // actually loaded (defaults have FullCouncil + Exclusive, not
    // FixedSize + ShareAfterSubmit). Prior to configured_project_root()
    // stripping the `.brehon` suffix, this test passed only by accident
    // because defaults happen to declare a panel named "primary".
    let review = load_project_review_config().expect("review config should load");
    assert_eq!(
        review.panel_mode,
        brehon_types::config::ReviewPanelMode::FixedSize
    );
    assert_eq!(
        review.lease_mode,
        brehon_types::config::ReviewLeaseMode::ShareAfterSubmit,
        "share_after_submit must survive the BREHON_ROOT-only env path; \
             regressing this silently disables reviewer session resets"
    );
    assert_eq!(review.panels.len(), 1);
    assert_eq!(review.panels[0].id, "primary");
    assert_eq!(
        review.panels[0].reviewers,
        vec!["codex-reviewer".to_string(), "gemini-reviewer".to_string()]
    );
}

#[test]
fn test_configured_project_root_strips_dot_brehon_suffix() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let brehon_dir = project_root.join(".brehon");
    fs::create_dir_all(&brehon_dir).unwrap();

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_dir.to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", ""),
    ]);
    std::env::remove_var("BREHON_PROJECT_ROOT");

    let resolved = configured_project_root().expect("project root resolved");
    assert_eq!(resolved, project_root);
}

#[test]
fn test_configured_project_root_prefers_explicit_project_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let brehon_dir = project_root.join(".brehon");
    fs::create_dir_all(&brehon_dir).unwrap();

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_dir.to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", project_root.to_str().unwrap()),
    ]);

    let resolved = configured_project_root().expect("project root resolved");
    assert_eq!(resolved, project_root);
}

#[test]
fn test_configured_project_root_passes_through_non_dot_brehon_root() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let custom_root = temp.path().join("custom-state-dir");
    fs::create_dir_all(&custom_root).unwrap();

    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", custom_root.to_str().unwrap()),
        ("BREHON_PROJECT_ROOT", ""),
    ]);
    std::env::remove_var("BREHON_PROJECT_ROOT");

    let resolved = configured_project_root().expect("project root resolved");
    assert_eq!(resolved, custom_root);
}

#[tokio::test]
async fn tool_panic_catches_panics_and_returns_internal_error() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[
        ("BREHON_SESSION_ID", "sess-tool-panic"),
        ("BREHON_AGENT_NAME", "panic-boundary-agent"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_tool(Box::new(PanicTool));

    let result = server.call_tool("panic_tool", serde_json::json!({})).await;
    match result {
        Err(McpError::Internal(message)) => {
            let payload = error_payload(&message);
            assert_eq!(payload["error_code"], "mcp_tool_panic");
            assert_eq!(payload["retryable"], true);
            assert_eq!(payload["current_state"]["tool"], "panic_tool");
            assert_eq!(
                payload["next_action"]["kind"],
                "retry_after_refresh_or_report"
            );
            assert_eq!(payload["error"]["code"], "mcp_tool_panic");
            assert_eq!(payload["error"]["tool"], "panic_tool");
            assert_eq!(payload["error"]["caller"]["session_id"], "sess-tool-panic");
            assert_eq!(
                payload["error"]["caller"]["agent_name"],
                "panic-boundary-agent"
            );
            assert_eq!(payload["error"]["caller"]["role"], "worker");
            assert!(payload["error"]["fields"]["panic"]
                .as_str()
                .unwrap()
                .contains("boom"));
        }
        other => panic!("expected internal panic error, got {other:?}"),
    }
}

#[tokio::test]
async fn input_size_rejects_oversized_arguments_with_caller_attribution() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[
        ("BREHON_MCP_MAX_REQUEST_BYTES", "64"),
        ("BREHON_SESSION_ID", "sess-oversize-test"),
        ("BREHON_AGENT_NAME", "oversize-test-agent"),
        ("BREHON_AGENT_ROLE", "reviewer"),
    ]);
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_tool(Box::new(EchoTool));

    let result = server
        .call_tool(
            "echo_tool",
            serde_json::json!({ "payload": "x".repeat(256) }),
        )
        .await;

    match result {
        Err(McpError::InvalidRequest(message)) => {
            let payload = error_payload(&message);
            assert_eq!(payload["error_code"], "mcp_input_too_large");
            assert_eq!(payload["retryable"], true);
            assert_eq!(payload["current_state"]["fields"]["max_bytes"], 64);
            assert_eq!(payload["next_action"]["kind"], "retry_with_smaller_request");
            assert!(!payload["allowed_next_actions"]
                .as_array()
                .unwrap()
                .is_empty());
            assert_eq!(payload["error"]["code"], "mcp_input_too_large");
            assert_eq!(payload["error"]["tool"], "echo_tool");
            assert_eq!(payload["error"]["fields"]["max_bytes"], 64);
            assert_eq!(
                payload["error"]["caller"]["session_id"],
                "sess-oversize-test"
            );
            assert_eq!(
                payload["error"]["caller"]["agent_name"],
                "oversize-test-agent"
            );
            assert_eq!(payload["error"]["caller"]["role"], "reviewer");
        }
        other => panic!("expected oversized payload rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn input_size_uses_cached_env_config_from_server_init() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[
        ("BREHON_MCP_MAX_REQUEST_BYTES", "64"),
        ("BREHON_SESSION_ID", "sess-initial"),
        ("BREHON_AGENT_NAME", "initial-agent"),
        ("BREHON_AGENT_ROLE", "worker"),
    ]);
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_tool(Box::new(EchoTool));

    std::env::set_var("BREHON_MCP_MAX_REQUEST_BYTES", "4096");
    std::env::set_var("BREHON_SESSION_ID", "sess-mutated-test");
    std::env::set_var("BREHON_AGENT_NAME", "updated-test-agent");
    std::env::set_var("BREHON_AGENT_ROLE", "reviewer");

    let result = server
        .call_tool(
            "echo_tool",
            serde_json::json!({ "payload": "x".repeat(256) }),
        )
        .await;

    match result {
        Err(McpError::InvalidRequest(message)) => {
            let payload = error_payload(&message);
            assert_eq!(payload["error"]["fields"]["max_bytes"], 64);
            assert_eq!(payload["error"]["caller"]["session_id"], "sess-initial");
            assert_eq!(payload["error"]["caller"]["agent_name"], "initial-agent");
            assert_eq!(payload["error"]["caller"]["role"], "worker");
            assert_ne!(
                payload["error"]["caller"]["session_id"],
                "sess-mutated-test"
            );
        }
        other => panic!("expected cached oversized payload rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn input_size_per_tool_bound_is_tighter_than_server_bound() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[
        ("BREHON_MCP_MAX_REQUEST_BYTES", "4096"),
        ("BREHON_SESSION_ID", "sess-tool-bound"),
    ]);
    let mut server = McpServer::new("test-server", "1.0.0");
    server.register_tool(Box::new(SmallBoundTool));

    let result = server
        .call_tool(
            "small_bound_tool",
            serde_json::json!({ "payload": "x".repeat(128) }),
        )
        .await;

    match result {
        Err(McpError::InvalidRequest(message)) => {
            let payload = error_payload(&message);
            assert_eq!(payload["error"]["fields"]["max_bytes"], 32);
            assert_eq!(payload["error"]["fields"]["server_max_bytes"], 4096);
            assert_eq!(payload["error"]["fields"]["tool_max_bytes"], 32);
        }
        other => panic!("expected per-tool oversized rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn input_size_bounded_transport_recovers_after_oversized_frame() {
    use tokio::io::{duplex, sink, AsyncWriteExt};

    let (mut writer, reader) = duplex(4096);
    let mut transport = BoundedServerTransport::new(reader, sink(), 128);
    writer.write_all(&vec![b'x'; 256]).await.unwrap();
    writer.write_all(b"\n").await.unwrap();

    let valid_message = RxJsonRpcMessage::<RoleServer>::request(
        rmcp::model::ClientRequest::from(rmcp::model::PingRequest::default()),
        rmcp::model::RequestId::Number(1),
    );
    let valid_bytes = serde_json::to_vec(&valid_message).unwrap();
    writer.write_all(&valid_bytes).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.shutdown().await.unwrap();

    let frame = transport.receive().await;
    assert!(
        frame.is_some(),
        "valid frame after oversized frame should be received"
    );

    let eof = transport.receive().await;
    assert!(eof.is_none(), "stream should eventually close at real EOF");
}
