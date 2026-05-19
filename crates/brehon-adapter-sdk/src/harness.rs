//! Harness types for Brehon adapter SDK.
//!
//! Defines the core types that describe how Brehon hosts and communicates
//! with agent processes: [`SupervisorCli`], [`HarnessCapabilities`],
//! [`HarnessTransport`], and [`HarnessControlPlane`].

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// How Brehon currently hosts a harness inside factory mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl HarnessControlPlane {
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

/// Supported interactive harnesses for factory panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
}

impl SupervisorCli {
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
                tool_prefix: Cow::Borrowed("mcp_brehon_"),
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
                tool_prefix: Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
        }
    }
}

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
            _ => Err(format!("unsupported harness: {s}")),
        }
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
    /// MCP tool name prefix used by this harness (e.g., `mcp__brehon__`).
    pub tool_prefix: Cow<'static, str>,
    /// How Brehon currently hosts this harness in factory mode.
    pub transport: HarnessTransport,
    /// The preferred structured control plane for this harness.
    pub preferred_control_plane: HarnessControlPlane,
}
