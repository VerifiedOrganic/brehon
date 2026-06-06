//! Harness types for Brehon adapter SDK.
//!
//! Defines the core types that describe how Brehon hosts and communicates
//! with agent processes: [`SupervisorCli`], [`HarnessCapabilities`],
//! [`HarnessTransport`], and [`HarnessControlPlane`].

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use brehon_types::agent::AdapterKind;
use strum::EnumCount;

/// How Brehon currently hosts a harness inside factory mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumCount)]
pub enum HarnessTransport {
    /// Native hook-driven integration (Claude).
    NativeHooks,
    /// Structured app-server transport (for compatible non-Claude workers).
    AppServer,
    /// Direct HTTP API managed inside Brehon.
    ManagedApi,
    /// Long-lived interactive PTY session.
    InteractivePty,
    /// Single-prompt PTY session that exits when done.
    OneShotPty,
}

impl HarnessTransport {
    /// All supported harness transports.
    pub const ALL: [Self; 5] = [
        Self::NativeHooks,
        Self::AppServer,
        Self::ManagedApi,
        Self::InteractivePty,
        Self::OneShotPty,
    ];

    /// Return the string representation of this transport.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeHooks => "native_hooks",
            Self::AppServer => "app_server",
            Self::ManagedApi => "managed_api",
            Self::InteractivePty => "interactive_pty",
            Self::OneShotPty => "one_shot_pty",
        }
    }

    /// Whether this transport is backed by a local PTY session.
    pub fn is_pty(self) -> bool {
        matches!(self, Self::InteractivePty | Self::OneShotPty)
    }

    /// Whether this transport is the one-shot PTY contract.
    pub fn is_one_shot(self) -> bool {
        matches!(self, Self::OneShotPty)
    }

    /// Whether this transport can carry the given control plane without
    /// diverging spawn and prompt-delivery behavior.
    pub fn supports_control_plane(self, control_plane: HarnessControlPlane) -> bool {
        matches!(
            (self, control_plane),
            (Self::NativeHooks, HarnessControlPlane::NativeHooks)
                | (Self::AppServer, HarnessControlPlane::Acp)
                | (Self::ManagedApi, HarnessControlPlane::OpenAiCompatible)
                | (
                    Self::InteractivePty,
                    HarnessControlPlane::AcpSidecar | HarnessControlPlane::PtyInjection
                )
                | (Self::OneShotPty, HarnessControlPlane::OneShot)
        )
    }
}

const _: [(); HarnessTransport::COUNT] = [(); HarnessTransport::ALL.len()];

impl fmt::Display for HarnessTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessTransport {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "native_hooks" => Ok(Self::NativeHooks),
            "app_server" => Ok(Self::AppServer),
            "managed_api" => Ok(Self::ManagedApi),
            "interactive_pty" => Ok(Self::InteractivePty),
            "one_shot_pty" => Ok(Self::OneShotPty),
            _ => Err(format!("unsupported harness transport: {s}")),
        }
    }
}

/// The preferred control plane Brehon should target for structured automation.
///
/// This is intentionally distinct from `HarnessTransport`: a worker can currently
/// run inside a PTY while still being a candidate for future ACP control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumCount)]
pub enum HarnessControlPlane {
    /// Claude's native hook/inbox model.
    NativeHooks,
    /// Agent Client Protocol or an ACP adapter.
    Acp,
    /// Agent Client Protocol exposed by an agent-owned Unix socket sidecar.
    AcpSidecar,
    /// Direct OpenAI-compatible API session behind AgentGateway.
    OpenAiCompatible,
    /// PTY text injection is the only available control surface.
    PtyInjection,
    /// One-shot prompt/response execution with process lifecycle as the boundary.
    OneShot,
}

const _: [(); HarnessControlPlane::COUNT] = [(); HarnessControlPlane::ALL.len()];

impl HarnessControlPlane {
    /// All supported harness control planes.
    pub const ALL: [Self; 6] = [
        Self::NativeHooks,
        Self::Acp,
        Self::AcpSidecar,
        Self::OpenAiCompatible,
        Self::PtyInjection,
        Self::OneShot,
    ];

    /// Return the string representation of this control plane.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeHooks => "native_hooks",
            Self::Acp => "acp",
            Self::AcpSidecar => "acp_sidecar",
            Self::OpenAiCompatible => "openai_compatible",
            Self::PtyInjection => "pty_injection",
            Self::OneShot => "one_shot",
        }
    }

    /// The canonical transport for this control plane when the control plane is
    /// the authoritative override signal.
    pub fn canonical_transport(self) -> HarnessTransport {
        match self {
            Self::NativeHooks => HarnessTransport::NativeHooks,
            Self::Acp => HarnessTransport::AppServer,
            Self::AcpSidecar | Self::PtyInjection => HarnessTransport::InteractivePty,
            Self::OpenAiCompatible => HarnessTransport::ManagedApi,
            Self::OneShot => HarnessTransport::OneShotPty,
        }
    }

    /// Whether this control plane expects Brehon to queue a synthetic startup
    /// prompt after spawn.
    pub fn needs_post_spawn_prompt(self) -> bool {
        matches!(
            self,
            Self::NativeHooks | Self::Acp | Self::AcpSidecar | Self::OpenAiCompatible
        )
    }

    /// Whether this control plane is the one-shot contract.
    pub fn is_one_shot(self) -> bool {
        matches!(self, Self::OneShot)
    }
}

impl fmt::Display for HarnessControlPlane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessControlPlane {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "native_hooks" => Ok(Self::NativeHooks),
            "acp" => Ok(Self::Acp),
            "acp_sidecar" => Ok(Self::AcpSidecar),
            "openai_compatible" => Ok(Self::OpenAiCompatible),
            "pty_injection" => Ok(Self::PtyInjection),
            "one_shot" => Ok(Self::OneShot),
            _ => Err(format!("unsupported harness control plane: {s}")),
        }
    }
}

/// How a PTY-backed harness expects prompt submission to be finalized.
///
/// This is related to [`HarnessCapabilities::uses_ink_prompt`], but the two
/// flags describe different concerns: `uses_ink_prompt` says whether Brehon
/// must wait for Ink/TUI readiness markers before injecting text, while
/// `PromptInjectionStrategy` says how submission is finalized once the prompt
/// has been written. `InkEcho` therefore implies an Ink-rendered UI, but
/// `ImmediateSubmit` can still coexist with `uses_ink_prompt = true` (for
/// example, Agy renders with Ink but submits immediately).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptInjectionStrategy {
    /// Write the prompt and press Enter immediately.
    ImmediateSubmit,
    /// Wait for an echoed Ink render before pressing Enter.
    InkEcho,
    /// Write the prompt, then press Enter after a short delay.
    DelayedSubmit,
}

impl PromptInjectionStrategy {
    /// Return the string representation of this strategy.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImmediateSubmit => "immediate_submit",
            Self::InkEcho => "ink_echo",
            Self::DelayedSubmit => "delayed_submit",
        }
    }

    /// Whether this strategy uses Ink echo detection before submit.
    pub fn uses_ink_echo(self) -> bool {
        matches!(self, Self::InkEcho)
    }

    /// Whether this strategy delays submit instead of pressing Enter immediately.
    pub fn uses_delayed_submit(self) -> bool {
        matches!(self, Self::DelayedSubmit)
    }
}

impl fmt::Display for PromptInjectionStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PromptInjectionStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "immediate_submit" => Ok(Self::ImmediateSubmit),
            "ink_echo" => Ok(Self::InkEcho),
            "delayed_submit" => Ok(Self::DelayedSubmit),
            _ => Err(format!("unsupported prompt injection strategy: {s}")),
        }
    }
}

/// Supported interactive harnesses for factory panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumCount)]
pub enum SupervisorCli {
    /// Anthropic Claude Code CLI.
    Claude,
    /// OpenAI Codex CLI.
    Codex,
    /// Google Gemini CLI.
    Gemini,
    /// Kimi Code CLI.
    Kimi,
    /// OpenCode CLI.
    OpenCode,
    /// JetBrains Junie CLI.
    Junie,
    /// GitHub Copilot CLI.
    Copilot,
    /// Google Antigravity 2.0 CLI.
    Agy,
}

impl SupervisorCli {
    /// All built-in CLI variants.
    pub const ALL: [Self; 8] = [
        Self::Claude,
        Self::Codex,
        Self::Gemini,
        Self::Kimi,
        Self::OpenCode,
        Self::Junie,
        Self::Copilot,
        Self::Agy,
    ];

    /// Return the string representation of this CLI type.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Kimi => "kimi",
            Self::OpenCode => "opencode",
            Self::Junie => "junie",
            Self::Copilot => "copilot",
            Self::Agy => "agy",
        }
    }

    /// Return the known capabilities for this built-in CLI.
    pub fn capabilities(self) -> HarnessCapabilities {
        match self {
            Self::Claude => HarnessCapabilities {
                supports_hooks: true,
                supports_subagents: true,
                supports_textbox_submit: true,
                supports_teams: true,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: Cow::Borrowed("mcp__brehon__"),
                transport: HarnessTransport::NativeHooks,
                preferred_control_plane: HarnessControlPlane::NativeHooks,
            },
            Self::Codex => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: true,
                prompt_injection_strategy: PromptInjectionStrategy::InkEcho,
                tool_prefix: Cow::Borrowed("mcp__brehon__"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            Self::Gemini => HarnessCapabilities {
                supports_hooks: true,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::DelayedSubmit,
                tool_prefix: Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            Self::Kimi => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: Cow::Borrowed(""),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            Self::OpenCode => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: true,
                prompt_injection_strategy: PromptInjectionStrategy::InkEcho,
                tool_prefix: Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            Self::Junie => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: true,
                prompt_injection_strategy: PromptInjectionStrategy::InkEcho,
                tool_prefix: Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::InteractivePty,
                preferred_control_plane: HarnessControlPlane::PtyInjection,
            },
            Self::Copilot => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            Self::Agy => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: true,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::InteractivePty,
                preferred_control_plane: HarnessControlPlane::PtyInjection,
            },
        }
    }

    /// Whether Brehon has an explicit spawn/runtime contract for this built-in
    /// CLI under the given effective transport/control-plane pair.
    pub fn supports_transport_control_plane(
        self,
        transport: HarnessTransport,
        control_plane: HarnessControlPlane,
    ) -> bool {
        match self {
            Self::Claude => matches!(
                (transport, control_plane),
                (
                    HarnessTransport::NativeHooks,
                    HarnessControlPlane::NativeHooks
                )
            ),
            Self::Junie | Self::Agy => matches!(
                (transport, control_plane),
                (
                    HarnessTransport::InteractivePty,
                    HarnessControlPlane::PtyInjection
                )
            ),
            Self::Codex | Self::Gemini | Self::Kimi | Self::OpenCode | Self::Copilot => {
                matches!(
                    (transport, control_plane),
                    (HarnessTransport::AppServer, HarnessControlPlane::Acp)
                        | (
                            HarnessTransport::InteractivePty,
                            HarnessControlPlane::PtyInjection
                        )
                )
            }
        }
    }
}

const _: [(); SupervisorCli::COUNT] = [(); SupervisorCli::ALL.len()];

impl FromStr for SupervisorCli {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "kimi" => Ok(Self::Kimi),
            "opencode" => Ok(Self::OpenCode),
            "junie" => Ok(Self::Junie),
            "copilot" | "gh-copilot" => Ok(Self::Copilot),
            "agy" => Ok(Self::Agy),
            _ => Err(format!("unsupported harness: {s}")),
        }
    }
}

/// Resolve a launcher configuration shape to a built-in [`SupervisorCli`].
///
/// This matcher is shared by config validation and runtime adapter selection so
/// arg-sensitive ACP launchers stay classified the same way in both places.
pub fn builtin_cli_from_launcher_shape(
    adapter: AdapterKind,
    command: Option<&str>,
    args: &[String],
) -> Option<SupervisorCli> {
    match adapter {
        AdapterKind::Codex => return Some(SupervisorCli::Codex),
        AdapterKind::Kimi => return Some(SupervisorCli::Kimi),
        AdapterKind::Junie => return Some(SupervisorCli::Junie),
        AdapterKind::Copilot => return Some(SupervisorCli::Copilot),
        AdapterKind::Agy => return Some(SupervisorCli::Agy),
        AdapterKind::Acp => {}
        _ => return None,
    }

    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    match (command.unwrap_or_default(), args.as_slice()) {
        ("claude", []) => Some(SupervisorCli::Claude),
        ("codex", ["app-server"]) => Some(SupervisorCli::Codex),
        ("gemini", ["--acp"]) | ("gemini", ["--experimental-acp"]) => Some(SupervisorCli::Gemini),
        ("kimi", ["acp"]) => Some(SupervisorCli::Kimi),
        ("opencode", [])
        | ("opencode", ["acp"])
        | ("opencode", ["acp", "--cwd", "."])
        | ("opencode", ["serve"])
        | ("opencode", ["serve", "--pure"]) => Some(SupervisorCli::OpenCode),
        ("junie", []) => Some(SupervisorCli::Junie),
        ("copilot", args) if args.is_empty() || args.contains(&"--acp") => {
            Some(SupervisorCli::Copilot)
        }
        _ => None,
    }
}

/// Capability flags and metadata for an agent harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCapabilities {
    /// Whether the CLI supports hook-based integration.
    pub supports_hooks: bool,
    /// Whether the CLI can spawn sub-agents.
    pub supports_subagents: bool,
    /// Whether the CLI supports textbox-style prompt submission.
    pub supports_textbox_submit: bool,
    /// Whether the agent reads Claude Code's native Teams inbox files.
    /// Only Claude Code polls `~/.claude/teams/*/inboxes/*.json`.
    pub supports_teams: bool,
    /// Whether the CLI exits after completing a single prompt (e.g.,
    /// `junie --task`, `gh copilot -p`). One-shot CLIs exiting with code 0 should
    /// not trigger crash recovery or respawn.
    pub one_shot: bool,
    /// Whether the agent renders an Ink-based TUI with a `>` prompt line.
    /// When true, the mux must detect prompt-readiness markers (empty `>`
    /// prompt, active turn indicator, etc.) before injecting text via PTY.
    pub uses_ink_prompt: bool,
    /// How PTY prompt injection should finalize prompt submission.
    pub prompt_injection_strategy: PromptInjectionStrategy,
    /// MCP tool name prefix used by this harness (e.g., `mcp__brehon__`).
    pub tool_prefix: Cow<'static, str>,
    /// How Brehon currently hosts this harness in factory mode.
    pub transport: HarnessTransport,
    /// The preferred structured control plane for this harness.
    pub preferred_control_plane: HarnessControlPlane,
}

#[cfg(test)]
mod tests {
    use super::{builtin_cli_from_launcher_shape, PromptInjectionStrategy, SupervisorCli};
    use brehon_types::agent::AdapterKind;

    #[test]
    fn builtin_cli_from_launcher_shape_requires_exact_codex_app_server_argv() {
        assert_eq!(
            builtin_cli_from_launcher_shape(
                AdapterKind::Acp,
                Some("codex"),
                &["app-server".to_string()],
            ),
            Some(SupervisorCli::Codex)
        );
        assert_eq!(
            builtin_cli_from_launcher_shape(
                AdapterKind::Acp,
                Some("codex"),
                &["app-server".to_string(), "--flag".to_string()],
            ),
            None
        );
    }

    #[test]
    fn builtin_cli_from_launcher_shape_keeps_acp_agy_custom() {
        assert_eq!(
            builtin_cli_from_launcher_shape(AdapterKind::Acp, Some("agy"), &[]),
            None
        );
        assert_eq!(
            builtin_cli_from_launcher_shape(
                AdapterKind::Acp,
                Some("agy"),
                &["--prompt-interactive".to_string()],
            ),
            None
        );
    }

    #[test]
    fn prompt_injection_strategy_round_trips() {
        for strategy in [
            PromptInjectionStrategy::ImmediateSubmit,
            PromptInjectionStrategy::InkEcho,
            PromptInjectionStrategy::DelayedSubmit,
        ] {
            assert_eq!(
                strategy
                    .as_str()
                    .parse::<PromptInjectionStrategy>()
                    .expect("strategy should parse"),
                strategy
            );
        }
    }

    #[test]
    fn kimi_uses_raw_mcp_tool_names() {
        let caps = SupervisorCli::Kimi.capabilities();
        assert_eq!(caps.tool_prefix.as_ref(), "");
    }

    #[test]
    fn prompt_injection_strategy_from_str_rejects_unknown() {
        assert_eq!(
            "bogus"
                .parse::<PromptInjectionStrategy>()
                .expect_err("unknown strategy should fail"),
            "unsupported prompt injection strategy: bogus"
        );
    }
}
