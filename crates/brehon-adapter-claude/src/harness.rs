//! Claude harness metadata and capability definitions.
//!
//! Re-exports and extends the types from [`brehon_adapter_sdk`] that are
//! specific to the Claude CLI harness, so that downstream crates
//! (like `brehon-pty` and `brehon-mux`) can reference them through this
//! adapter crate instead of hard-coding Claude-specific constants.

pub use brehon_adapter_sdk::{
    HarnessCapabilities, HarnessControlPlane, HarnessTransport, SupervisorCli,
};

use std::borrow::Cow;

/// The CLI binary name for Claude.
pub const CLAUDE_CLI_BINARY: &str = "claude";

/// The MCP tool name prefix that Claude uses for Brehon coordination.
pub const CLAUDE_TOOL_PREFIX: &str = "mcp__brehon__";

/// Return the [`HarnessCapabilities`] for the Claude CLI.
///
/// This is the canonical source of truth for Claude's capabilities;
/// it mirrors the values in
/// [`SupervisorCli::capabilities`](brehon_adapter_sdk::SupervisorCli::capabilities)
/// for the `Claude` variant but is available without going through the enum.
pub fn claude_capabilities() -> HarnessCapabilities {
    HarnessCapabilities {
        supports_hooks: true,
        supports_subagents: true,
        supports_textbox_submit: true,
        supports_teams: true,
        one_shot: false,
        uses_ink_prompt: false,
        tool_prefix: Cow::Borrowed(CLAUDE_TOOL_PREFIX),
        transport: HarnessTransport::NativeHooks,
        preferred_control_plane: HarnessControlPlane::NativeHooks,
    }
}

/// Default reasoning effort for a given role.
pub fn default_reasoning_effort(role: &str) -> &'static str {
    if role == "supervisor" {
        "high"
    } else {
        "medium"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_capabilities_matches_sdk() {
        let from_sdk = SupervisorCli::Claude.capabilities();
        let from_adapter = claude_capabilities();
        assert_eq!(from_sdk.supports_hooks, from_adapter.supports_hooks);
        assert_eq!(from_sdk.supports_subagents, from_adapter.supports_subagents);
        assert_eq!(
            from_sdk.supports_textbox_submit,
            from_adapter.supports_textbox_submit
        );
        assert_eq!(from_sdk.supports_teams, from_adapter.supports_teams);
        assert_eq!(from_sdk.one_shot, from_adapter.one_shot);
        assert_eq!(from_sdk.uses_ink_prompt, from_adapter.uses_ink_prompt);
        assert_eq!(
            from_sdk.preferred_control_plane,
            from_adapter.preferred_control_plane
        );
        assert_eq!(from_sdk.transport, from_adapter.transport);
    }

    #[test]
    fn default_effort_high_for_supervisor() {
        assert_eq!(default_reasoning_effort("supervisor"), "high");
        assert_eq!(default_reasoning_effort("worker"), "medium");
        assert_eq!(default_reasoning_effort("reviewer"), "medium");
    }
}
