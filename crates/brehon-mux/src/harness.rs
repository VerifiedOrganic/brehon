pub use brehon_adapter_sdk::harness::{
    HarnessCapabilities, HarnessControlPlane, HarnessTransport, SupervisorCli,
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
    /// A user-defined agent loaded from configuration.
    Custom(CustomAgentConfig),
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
    /// Environment variable containing the API key for direct adapters.
    pub api_key_env: Option<String>,
    /// Extra static headers for direct adapters.
    pub headers: Vec<(String, String)>,
    /// Capabilities of this custom agent.
    pub capabilities: HarnessCapabilities,
}

impl AgentAdapter {
    /// Human-readable name for this adapter.
    pub fn name(&self) -> &str {
        match self {
            Self::BuiltIn(cli) => cli.as_str(),
            Self::Custom(cfg) => &cfg.name,
        }
    }

    /// Resolve capabilities for this adapter.
    pub fn capabilities(&self) -> HarnessCapabilities {
        match self {
            Self::BuiltIn(cli) => cli.capabilities(),
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
            Self::Custom(_) => None,
        }
    }

    /// Return the preferred control plane for this adapter.
    pub fn control_plane(&self) -> HarnessControlPlane {
        self.capabilities().preferred_control_plane
    }

    /// Whether Brehon should queue a synthetic startup prompt after spawn.
    pub fn needs_post_spawn_prompt(&self) -> bool {
        matches!(self.as_builtin(), Some(SupervisorCli::Claude))
            || matches!(
                self.control_plane(),
                HarnessControlPlane::Acp
                    | HarnessControlPlane::AcpSidecar
                    | HarnessControlPlane::OpenAiCompatible
            )
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
    fn gemini_needs_post_spawn_prompt_by_default() {
        let adapter = AgentAdapter::BuiltIn(SupervisorCli::Gemini);
        assert!(adapter.needs_post_spawn_prompt());
    }
}
