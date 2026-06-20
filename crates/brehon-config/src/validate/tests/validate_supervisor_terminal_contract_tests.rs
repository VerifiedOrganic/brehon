use super::*;

#[test]
fn supervisor_terminal_contract_rejects_gateway_only_acp_launcher() {
    let mut config = minimal_valid_config();
    config.launchers.insert(
        "goose".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::Acp,
            command: Some("goose".into()),
            args: vec!["acp".into()],
            provider: None,
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    config.lanes.insert(
        "goose-supervisor".into(),
        LaneConfig {
            launcher: "goose".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );
    config.roles.supervisor.name = "goose-supervisor".into();

    let warnings = validate(&config);

    assert!(warnings.iter().any(|warning| {
        warning.kind == ValidationWarningKind::SupervisorTerminalContract
            && warning.is_fatal
            && warning.message.contains("Gateway-only ACP/API launchers")
    }));
}

#[test]
fn supervisor_terminal_contract_rejects_openai_launcher() {
    let mut config = minimal_valid_config();
    config.launchers.insert(
        "openai".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::OpenAiCompatible,
            command: None,
            args: Vec::new(),
            provider: None,
            transport: None,
            control_plane: None,
            base_url: Some("https://api.openai.example/v1".into()),
            api_key_env: Some("OPENAI_API_KEY".into()),
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    config.lanes.insert(
        "api-supervisor".into(),
        LaneConfig {
            launcher: "openai".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );
    config.roles.supervisor.name = "api-supervisor".into();

    let warnings = validate(&config);

    assert!(warnings.iter().any(|warning| {
        warning.kind == ValidationWarningKind::SupervisorTerminalContract
            && warning.is_fatal
            && warning.message.contains("interactive PTY-backed")
    }));
}

#[test]
fn supervisor_terminal_contract_accepts_custom_pty_launcher() {
    let mut config = minimal_valid_config();
    config.launchers.insert(
        "goose-pty".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::PtyHooks,
            command: Some("goose".into()),
            args: vec!["--interactive".into()],
            provider: None,
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    config.lanes.insert(
        "goose-supervisor".into(),
        LaneConfig {
            launcher: "goose-pty".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );
    config.roles.supervisor.name = "goose-supervisor".into();

    let warnings = validate(&config);

    assert!(!warnings
        .iter()
        .any(|warning| warning.kind == ValidationWarningKind::SupervisorTerminalContract));
}

#[test]
fn supervisor_terminal_contract_rejects_opencode_acp_launcher() {
    let mut config = minimal_valid_config();
    config.launchers.insert(
        "opencode-acp-supervisor".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::Acp,
            command: Some("opencode".into()),
            args: vec![],
            provider: None,
            transport: Some(HarnessTransport::AppServer.to_string()),
            control_plane: Some(HarnessControlPlane::Acp.to_string()),
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    config.lanes.insert(
        "opencode-supervisor".into(),
        LaneConfig {
            launcher: "opencode-acp-supervisor".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );
    config.roles.supervisor.name = "opencode-supervisor".into();

    let warnings = validate(&config);

    assert!(warnings.iter().any(|warning| {
        warning.kind == ValidationWarningKind::SupervisorTerminalContract
            && warning.is_fatal
            && warning
                .message
                .contains("resolves OpenCode to non-PTY supervisor transport/control plane")
    }));
}

#[test]
fn supervisor_terminal_contract_rejects_default_opencode_acp_launcher() {
    let mut config = minimal_valid_config();
    config.launchers.insert(
        "opencode-acp-supervisor".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::Acp,
            command: Some("opencode".into()),
            args: vec![],
            provider: None,
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    config.lanes.insert(
        "opencode-supervisor".into(),
        LaneConfig {
            launcher: "opencode-acp-supervisor".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );
    config.roles.supervisor.name = "opencode-supervisor".into();

    let warnings = validate(&config);

    assert!(warnings.iter().any(|warning| {
        warning.kind == ValidationWarningKind::SupervisorTerminalContract
            && warning.is_fatal
            && warning
                .message
                .contains("resolves OpenCode to non-PTY supervisor transport/control plane")
    }));
}

#[test]
fn supervisor_terminal_contract_accepts_opencode_pty_launcher() {
    let mut config = minimal_valid_config();
    config.launchers.insert(
        "opencode-pty-supervisor".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::Acp,
            command: Some("opencode".into()),
            args: vec![],
            provider: None,
            transport: Some(HarnessTransport::InteractivePty.to_string()),
            control_plane: Some(HarnessControlPlane::PtyInjection.to_string()),
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    config.lanes.insert(
        "opencode-supervisor".into(),
        LaneConfig {
            launcher: "opencode-pty-supervisor".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );
    config.roles.supervisor.name = "opencode-supervisor".into();

    let warnings = validate(&config);

    assert!(!warnings
        .iter()
        .any(|warning| warning.kind == ValidationWarningKind::SupervisorTerminalContract));
}

#[test]
fn supervisor_terminal_contract_accepts_acp_sidecar_launcher() {
    let mut config = minimal_valid_config();
    config.launchers.insert(
        "native-supervisor".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::Acp,
            command: Some("brehon-native-agent".into()),
            args: vec!["--supervised".into()],
            provider: None,
            transport: Some("interactive_pty".into()),
            control_plane: Some("acp_sidecar".into()),
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    config.lanes.insert(
        "native-supervisor".into(),
        LaneConfig {
            launcher: "native-supervisor".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );
    config.roles.supervisor.name = "native-supervisor".into();

    let warnings = validate(&config);

    assert!(!warnings
        .iter()
        .any(|warning| warning.kind == ValidationWarningKind::SupervisorTerminalContract));
}
