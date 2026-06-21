use brehon_types::BrehonConfig;

pub(super) fn opencode_supervisor_adapter(
    name: &str,
    config: &BrehonConfig,
    cli: brehon_mux::SupervisorCli,
) -> Option<brehon_mux::AgentAdapter> {
    if cli != brehon_mux::SupervisorCli::OpenCode || config.roles.supervisor.name != name {
        return None;
    }

    let mut capabilities = cli.capabilities();
    if let Some(agent_config) = config.lane_launcher(name) {
        super::apply_capability_overrides(&mut capabilities, agent_config);
    }
    capabilities.transport = brehon_mux::HarnessTransport::InteractivePty;
    capabilities.preferred_control_plane = brehon_mux::HarnessControlPlane::PtyInjection;
    Some(brehon_mux::AgentAdapter::built_in_with_capabilities(
        cli,
        capabilities,
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_agent_to_adapter_forces_opencode_supervisor_to_pty() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "opencode-supervisor".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("opencode".to_string()),
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
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );
        config.lanes.insert(
            "opencode-supervisor".to_string(),
            brehon_types::LaneConfig {
                launcher: "opencode-supervisor".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "opencode-supervisor".to_string();

        let adapter = super::super::agent_to_adapter("opencode-supervisor", &config);
        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::OpenCode)
        );
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::InteractivePty
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::PtyInjection
        );
    }
}
