pub use brehon_adapter_sdk::harness::{
    HarnessCapabilities, HarnessControlPlane, HarnessTransport, PromptInjectionStrategy,
    SupervisorCli, builtin_cli_from_launcher_shape,
};

use std::fmt;
use std::str::FromStr;

/// Agent adapter that wraps built-in CLI types and custom agent configurations.
///
/// This is the primary type used throughout brehon-mux and brehon-factory to
/// refer to an agent's identity and capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAdapter {
    /// A built-in harness with known capabilities.
    BuiltIn(SupervisorCli),
    /// A built-in harness that keeps its launch contract while overriding
    /// selected runtime capabilities such as transport/control plane.
    BuiltInOverride(BuiltInOverrideConfig),
    /// A user-defined agent loaded from configuration.
    Custom(CustomAgentConfig),
}

/// Capability overrides for a built-in harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInOverrideConfig {
    /// The built-in CLI whose launch/runtime contract this adapter still uses.
    pub cli: SupervisorCli,
    /// Effective capabilities after applying launcher overrides.
    pub capabilities: HarnessCapabilities,
}

/// Configuration for a custom (non-built-in) agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomAgentConfig {
    /// Display name for this agent (e.g., "aider", "mentat").
    pub name: String,
    /// Shell command to launch the agent for subprocess-backed adapters.
    pub command: Option<String>,
    /// Command-line arguments for subprocess-backed adapters.
    pub args: Vec<String>,
    /// Base URL for direct OpenAI-compatible adapters.
    pub base_url: Option<String>,
    /// Max concurrent in-flight requests Brehon routes to this endpoint, shared
    /// across every lane pointed at the same `base_url`. `None` = unlimited.
    pub max_concurrency: Option<usize>,
    /// Environment variable containing the API key for direct adapters.
    pub api_key_env: Option<String>,
    /// Extra static headers for direct adapters.
    pub headers: Vec<(String, String)>,
    /// Capabilities of this custom agent.
    pub capabilities: HarnessCapabilities,
}

impl CustomAgentConfig {
    /// Return the preferred control plane for this custom agent.
    pub fn control_plane(&self) -> HarnessControlPlane {
        self.capabilities.preferred_control_plane
    }
}

impl BuiltInOverrideConfig {
    /// Return the preferred control plane for this built-in override.
    pub fn control_plane(&self) -> HarnessControlPlane {
        self.capabilities.preferred_control_plane
    }
}

impl AgentAdapter {
    /// Construct a built-in adapter with optional capability overrides.
    ///
    /// Unsupported transport/control-plane pairs are normalized back to the
    /// built-in CLI's canonical launch contract before the override is stored.
    pub fn built_in_with_capabilities(
        cli: SupervisorCli,
        mut capabilities: HarnessCapabilities,
    ) -> Self {
        let requests_one_shot = capabilities.one_shot
            || capabilities.transport.is_one_shot()
            || capabilities.preferred_control_plane.is_one_shot();
        if !requests_one_shot
            && !cli.supports_transport_control_plane(
                capabilities.transport,
                capabilities.preferred_control_plane,
            )
        {
            let builtin = cli.capabilities();
            capabilities.transport = builtin.transport;
            capabilities.preferred_control_plane = builtin.preferred_control_plane;
            capabilities.one_shot = builtin.one_shot;
        }
        if capabilities == cli.capabilities() {
            Self::BuiltIn(cli)
        } else {
            Self::BuiltInOverride(BuiltInOverrideConfig { cli, capabilities })
        }
    }

    /// Human-readable name for this adapter.
    pub fn name(&self) -> &str {
        match self {
            Self::BuiltIn(cli) => cli.as_str(),
            Self::BuiltInOverride(cfg) => cfg.cli.as_str(),
            Self::Custom(cfg) => &cfg.name,
        }
    }

    /// Resolve capabilities for this adapter.
    pub fn capabilities(&self) -> HarnessCapabilities {
        match self {
            Self::BuiltIn(cli) => cli.capabilities(),
            Self::BuiltInOverride(cfg) => cfg.capabilities.clone(),
            Self::Custom(cfg) => cfg.capabilities.clone(),
        }
    }

    /// Alias for `name()` — compatibility with legacy API.
    pub fn as_str(&self) -> &str {
        self.name()
    }

    /// If this is a built-in adapter, return the inner `SupervisorCli`.
    pub fn as_builtin(&self) -> Option<SupervisorCli> {
        match self {
            Self::BuiltIn(cli) => Some(*cli),
            Self::BuiltInOverride(cfg) => Some(cfg.cli),
            Self::Custom(_) => None,
        }
    }

    /// Return the preferred control plane for this adapter.
    pub fn control_plane(&self) -> HarnessControlPlane {
        match self {
            Self::BuiltIn(cli) => cli.capabilities().preferred_control_plane,
            Self::BuiltInOverride(cfg) => cfg.control_plane(),
            Self::Custom(cfg) => cfg.control_plane(),
        }
    }

    /// Whether Brehon should queue a synthetic startup prompt after spawn.
    ///
    /// This is keyed by the adapter's [`HarnessControlPlane`] capability, not
    /// by CLI name. Any adapter — built-in or custom — whose control plane is
    /// `NativeHooks`, `Acp`, `AcpSidecar`, or `OpenAiCompatible` receives a
    /// synthetic startup prompt. PTY-only adapters (`PtyInjection`, `OneShot`)
    /// rely on embedded startup prompts or process lifecycle boundaries and do
    /// not need one.
    pub fn needs_post_spawn_prompt(&self) -> bool {
        self.control_plane().needs_post_spawn_prompt()
    }
}

impl fmt::Display for AgentAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl From<SupervisorCli> for AgentAdapter {
    fn from(cli: SupervisorCli) -> Self {
        Self::BuiltIn(cli)
    }
}

/// Parse a built-in adapter by string alias.
///
/// String parsing only resolves canonical built-in launcher names. Capability
/// overrides are applied later during config/launcher resolution via
/// [`AgentAdapter::built_in_with_capabilities`], so `FromStr` never produces
/// [`AgentAdapter::BuiltInOverride`].
impl FromStr for AgentAdapter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Try built-in first; custom agents are resolved via AgentRegistry.
        match SupervisorCli::from_str(s) {
            Ok(cli) => Ok(Self::BuiltIn(cli)),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::EnumCount;

    #[test]
    fn claude_needs_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Claude);
        assert!(adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn codex_needs_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Codex);
        assert!(adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn gemini_needs_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Gemini);
        assert!(adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn kimi_needs_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Kimi);
        assert!(adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn opencode_needs_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::OpenCode);
        assert!(adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn copilot_needs_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Copilot);
        assert!(adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn junie_does_not_need_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Junie);
        assert!(!adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn agy_does_not_need_post_spawn_prompt() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Agy);
        assert!(!adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn custom_native_hooks_needs_post_spawn_prompt() {
        let adapter = AgentAdapter::Custom(CustomAgentConfig {
            name: "custom-native".into(),
            command: Some("custom".into()),
            args: vec![],
            base_url: None,
            max_concurrency: None,
            api_key_env: None,
            headers: vec![],
            capabilities: HarnessCapabilities {
                supports_hooks: true,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
                transport: HarnessTransport::NativeHooks,
                preferred_control_plane: HarnessControlPlane::NativeHooks,
            },
        });
        assert!(adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn custom_pty_injection_does_not_need_post_spawn_prompt() {
        let adapter = AgentAdapter::Custom(CustomAgentConfig {
            name: "custom-pty".into(),
            command: Some("custom".into()),
            args: vec![],
            base_url: None,
            max_concurrency: None,
            api_key_env: None,
            headers: vec![],
            capabilities: HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: true,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
                transport: HarnessTransport::InteractivePty,
                preferred_control_plane: HarnessControlPlane::PtyInjection,
            },
        });
        assert!(!adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn built_in_override_preserves_builtin_identity() {
        let mut capabilities = SupervisorCli::Gemini.capabilities();
        capabilities.transport = HarnessTransport::InteractivePty;
        capabilities.preferred_control_plane = HarnessControlPlane::PtyInjection;

        let adapter = AgentAdapter::built_in_with_capabilities(SupervisorCli::Gemini, capabilities);

        assert_eq!(adapter.as_builtin(), Some(SupervisorCli::Gemini));
        assert_eq!(adapter.name(), "gemini");
        assert_eq!(
            adapter.capabilities().transport,
            HarnessTransport::InteractivePty
        );
        assert_eq!(adapter.control_plane(), HarnessControlPlane::PtyInjection);
    }

    #[test]
    fn built_in_override_pty_does_not_need_post_spawn_prompt() {
        let mut capabilities = SupervisorCli::Gemini.capabilities();
        capabilities.transport = HarnessTransport::InteractivePty;
        capabilities.preferred_control_plane = HarnessControlPlane::PtyInjection;

        let adapter = AgentAdapter::built_in_with_capabilities(SupervisorCli::Gemini, capabilities);

        assert!(!adapter.needs_post_spawn_prompt());
    }

    #[test]
    fn built_in_override_normalizes_unsupported_transport_control_plane_pair() {
        let mut capabilities = SupervisorCli::Claude.capabilities();
        capabilities.transport = HarnessTransport::AppServer;
        capabilities.preferred_control_plane = HarnessControlPlane::Acp;

        let adapter = AgentAdapter::built_in_with_capabilities(SupervisorCli::Claude, capabilities);

        assert_eq!(adapter, AgentAdapter::BuiltIn(SupervisorCli::Claude));
        assert_eq!(adapter.capabilities(), SupervisorCli::Claude.capabilities());
    }

    #[test]
    fn from_str_only_produces_builtin_adapters() {
        let adapter = "gemini".parse::<AgentAdapter>().expect("parse built-in");

        assert_eq!(adapter, AgentAdapter::BuiltIn(SupervisorCli::Gemini));
    }

    #[test]
    fn all_built_in_adapters_covered_by_post_spawn_prompt_test() {
        assert_eq!(
            SupervisorCli::ALL.len(),
            SupervisorCli::COUNT,
            "update SupervisorCli::ALL when adding new built-in variants"
        );
        for cli in SupervisorCli::ALL {
            let adapter = AgentAdapter::BuiltIn(cli);
            // We just assert that the method does not panic and that the
            // result is consistent with the control plane capability.
            let result = adapter.needs_post_spawn_prompt();
            let cp = adapter.control_plane();
            let expected = cp.needs_post_spawn_prompt();
            assert_eq!(
                result,
                expected,
                "{}: needs_post_spawn_prompt() mismatch for control plane {cp}",
                cli.as_str()
            );
        }
    }
}
