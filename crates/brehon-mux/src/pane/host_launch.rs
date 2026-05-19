//! Terminal-host launch planning for agent panes.

use std::collections::BTreeMap;

use brehon_acp::GatewayProtocol;
use brehon_types::{RuntimeCommandKind, RuntimePaneKind, TerminalPaneSpawnSpec};

use crate::pane::types::{GatewaySpawnConfig, Pane, PaneKind};
use crate::pty::PtyConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTerminalLaunchSpec {
    pub spec: TerminalPaneSpawnSpec,
}

impl AgentTerminalLaunchSpec {
    pub fn to_runtime_spawn_command(&self) -> RuntimeCommandKind {
        RuntimeCommandKind::SpawnPane {
            kind: self.spec.kind.clone(),
            pane_id: Some(self.spec.pane_id.clone()),
            title: self.spec.title.clone(),
            cwd: self.spec.cwd.clone(),
            command: self.spec.command.clone(),
            env: self.spec.env.clone(),
            rows: Some(self.spec.rows),
            cols: Some(self.spec.cols),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTerminalLaunchPlan {
    TerminalHost(AgentTerminalLaunchSpec),
    GatewayBacked {
        protocol: &'static str,
        reason: String,
    },
    Unsupported {
        reason: String,
    },
}

impl AgentTerminalLaunchPlan {
    pub fn from_pty_config(
        session_id: impl Into<String>,
        pane_id: impl Into<String>,
        title: Option<String>,
        kind: RuntimePaneKind,
        config: &PtyConfig,
    ) -> Self {
        let mut command = Vec::with_capacity(config.args.len().saturating_add(1));
        command.push(config.command.clone());
        command.extend(config.args.clone());

        Self::TerminalHost(AgentTerminalLaunchSpec {
            spec: TerminalPaneSpawnSpec {
                session_id: session_id.into(),
                pane_id: pane_id.into(),
                kind,
                title,
                cwd: config
                    .cwd
                    .as_ref()
                    .map(|cwd| cwd.to_string_lossy().to_string()),
                command,
                env: config.env.iter().cloned().collect::<BTreeMap<_, _>>(),
                rows: config.rows,
                cols: config.cols,
            },
        })
    }

    pub fn is_terminal_host_eligible(&self) -> bool {
        matches!(self, Self::TerminalHost(_))
    }

    pub fn promotion_blocker(&self) -> Option<&str> {
        match self {
            Self::TerminalHost(_) => None,
            Self::GatewayBacked { reason, .. } | Self::Unsupported { reason } => Some(reason),
        }
    }
}

impl Pane {
    pub fn terminal_host_launch_plan(&self, session_id: &str) -> AgentTerminalLaunchPlan {
        if let Some(config) = self.pty_spawn_config.as_ref() {
            return AgentTerminalLaunchPlan::from_pty_config(
                session_id,
                self.id.clone(),
                Some(self.title.clone()),
                runtime_pane_kind(&self.kind),
                config,
            );
        }

        if let Some(config) = self.gateway_spawn_config.as_ref() {
            return gateway_backed_plan(config);
        }

        AgentTerminalLaunchPlan::Unsupported {
            reason: format!(
                "{} pane '{}' has no restartable PTY or gateway launch contract",
                self.kind.as_str(),
                self.id
            ),
        }
    }
}

fn runtime_pane_kind(kind: &PaneKind) -> RuntimePaneKind {
    match kind {
        PaneKind::Worker => RuntimePaneKind::Worker,
        PaneKind::Supervisor => RuntimePaneKind::Supervisor,
        PaneKind::Director => RuntimePaneKind::Director,
        PaneKind::Shell => RuntimePaneKind::Shell,
        PaneKind::Reviewer => RuntimePaneKind::Reviewer,
        PaneKind::Advisor => RuntimePaneKind::Advisor,
    }
}

fn gateway_backed_plan(config: &GatewaySpawnConfig) -> AgentTerminalLaunchPlan {
    AgentTerminalLaunchPlan::GatewayBacked {
        protocol: gateway_protocol_label(config.protocol),
        reason: format!(
            "gateway-backed {protocol} agent sessions are not terminal-host PTY panes",
            protocol = gateway_protocol_label(config.protocol)
        ),
    }
}

fn gateway_protocol_label(protocol: GatewayProtocol) -> &'static str {
    match protocol {
        GatewayProtocol::AcpStdio => "acp_stdio",
        GatewayProtocol::AcpUnixSocket => "acp_unix_socket",
        GatewayProtocol::GeminiAcpStdio => "gemini_acp_stdio",
        GatewayProtocol::CopilotAcpStdio => "copilot_acp_stdio",
        GatewayProtocol::KimiAcpStdio => "kimi_acp_stdio",
        GatewayProtocol::CodexAppServerWs => "codex_app_server_ws",
        GatewayProtocol::OpenCodeServer => "opencode_server",
        GatewayProtocol::OpenAiCompatibleChat => "openai_compatible_chat",
        GatewayProtocol::JunieStdio => "junie_stdio",
    }
}
