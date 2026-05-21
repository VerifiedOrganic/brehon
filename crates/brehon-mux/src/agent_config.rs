//! Agent registry for resolving agent names to adapters.
//!
//! Built-in agents are resolved by name; custom agents can be registered
//! from TOML-style configuration.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::harness::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane, HarnessTransport,
    SupervisorCli,
};

/// Registry that resolves agent names to `AgentAdapter` instances.
///
/// Built-in agents (claude, codex, gemini, kimi, opencode, junie, copilot) are
/// always available. Custom agents can be added from configuration.
#[derive(Debug, Clone)]
pub struct AgentRegistry {
    custom: HashMap<String, CustomAgentConfig>,
}

impl AgentRegistry {
    /// Create an empty registry with no custom agents.
    pub fn new() -> Self {
        Self {
            custom: HashMap::new(),
        }
    }

    /// Register a custom agent configuration.
    pub fn register(&mut self, config: CustomAgentConfig) {
        self.custom.insert(config.name.clone(), config);
    }

    /// Resolve an agent name to an `AgentAdapter`.
    ///
    /// Tries built-in names first, then custom registrations.
    pub fn resolve(&self, name: &str) -> Option<AgentAdapter> {
        // Built-in first
        if let Ok(cli) = name.parse::<SupervisorCli>() {
            return Some(AgentAdapter::BuiltIn(cli));
        }
        // Custom
        self.custom
            .get(name)
            .map(|cfg| AgentAdapter::Custom(cfg.clone()))
    }

    /// List all known agent names (built-in + custom).
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = vec![
            "claude", "codex", "gemini", "kimi", "opencode", "junie", "copilot", "agy",
        ];
        names.extend(self.custom.keys().map(|s| s.as_str()));
        names
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `CustomAgentConfig` from TOML-like key-value fields.
///
/// Expected fields: name, command, args (comma-separated), transport,
/// control_plane, tool_prefix. Missing capability fields default to
/// conservative values (all false, PtyInjection control plane).
pub fn custom_agent_from_fields(fields: &HashMap<String, String>) -> Option<CustomAgentConfig> {
    let name = fields.get("name")?.clone();
    let command = fields.get("command")?.clone();
    let args = fields
        .get("args")
        .map(|a| a.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let transport = fields
        .get("transport")
        .and_then(|s| s.parse::<HarnessTransport>().ok())
        .unwrap_or(HarnessTransport::InteractivePty);

    let control_plane = fields
        .get("control_plane")
        .and_then(|s| s.parse::<HarnessControlPlane>().ok())
        .unwrap_or(HarnessControlPlane::PtyInjection);

    let tool_prefix = fields
        .get("tool_prefix")
        .cloned()
        .unwrap_or_else(|| "mcp_brehon_".to_string());

    let one_shot = fields.get("one_shot").map(|v| v == "true").unwrap_or(false);

    Some(CustomAgentConfig {
        name,
        command: Some(command),
        args,
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: fields
                .get("supports_textbox_submit")
                .map(|v| v == "true")
                .unwrap_or(true),
            supports_teams: false,
            one_shot,
            uses_ink_prompt: fields
                .get("uses_ink_prompt")
                .map(|v| v == "true")
                .unwrap_or(false),
            tool_prefix: Cow::Owned(tool_prefix),
            transport,
            preferred_control_plane: control_plane,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_builtin() {
        let reg = AgentRegistry::new();
        let adapter = reg.resolve("claude").unwrap();
        assert_eq!(adapter.name(), "claude");
        assert!(adapter.as_builtin().is_some());
    }

    #[test]
    fn resolve_custom() {
        let mut reg = AgentRegistry::new();
        reg.register(CustomAgentConfig {
            name: "aider".to_string(),
            command: Some("aider".to_string()),
            args: vec!["--yes".to_string()],
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            capabilities: HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                tool_prefix: Cow::Owned("mcp_brehon_".to_string()),
                transport: HarnessTransport::InteractivePty,
                preferred_control_plane: HarnessControlPlane::PtyInjection,
            },
        });
        let adapter = reg.resolve("aider").unwrap();
        assert_eq!(adapter.name(), "aider");
        assert!(adapter.as_builtin().is_none());
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let reg = AgentRegistry::new();
        assert!(reg.resolve("unknown_agent").is_none());
    }

    #[test]
    fn builtin_takes_priority() {
        let mut reg = AgentRegistry::new();
        // Even if someone registers "claude" as custom, built-in wins
        reg.register(CustomAgentConfig {
            name: "claude".to_string(),
            command: Some("my-claude".to_string()),
            args: vec![],
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            capabilities: HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                tool_prefix: Cow::Borrowed("custom_"),
                transport: HarnessTransport::InteractivePty,
                preferred_control_plane: HarnessControlPlane::PtyInjection,
            },
        });
        let adapter = reg.resolve("claude").unwrap();
        assert!(adapter.as_builtin().is_some());
    }

    #[test]
    fn custom_agent_from_fields_basic() {
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "mentat".to_string());
        fields.insert("command".to_string(), "mentat".to_string());
        fields.insert("args".to_string(), "--auto, --model gpt-4".to_string());
        fields.insert("transport".to_string(), "interactive_pty".to_string());

        let config = custom_agent_from_fields(&fields).unwrap();
        assert_eq!(config.name, "mentat");
        assert_eq!(config.args, vec!["--auto", "--model gpt-4"]);
        assert_eq!(
            config.capabilities.transport,
            HarnessTransport::InteractivePty
        );
    }

    #[test]
    fn custom_agent_from_fields_accepts_acp_sidecar_control_plane() {
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "native-agent".to_string());
        fields.insert("command".to_string(), "native-agent".to_string());
        fields.insert("transport".to_string(), "interactive_pty".to_string());
        fields.insert("control_plane".to_string(), "acp_sidecar".to_string());

        let config = custom_agent_from_fields(&fields).unwrap();
        assert_eq!(
            config.capabilities.transport,
            HarnessTransport::InteractivePty
        );
        assert_eq!(
            config.capabilities.preferred_control_plane,
            HarnessControlPlane::AcpSidecar
        );
    }
}
