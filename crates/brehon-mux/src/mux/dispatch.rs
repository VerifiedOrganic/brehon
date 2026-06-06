//! Per-CLI prompt and input dispatch between ACP gateway, Teams inbox, and PTY.
//!
//! Routing decisions are driven by `AgentAdapter` capabilities rather than
//! `SupervisorCli` variants, so adding a new CLI does not require editing
//! this file. Capabilities like `supports_teams`, `preferred_control_plane`,
//! and `transport` determine which delivery path a pane takes.

use crate::error::{Error, Result};
use crate::harness::HarnessControlPlane;
use crate::pane::{
    ClaudePromptState, DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP, DeathReason, GatewaySpawnConfig,
    Generation, PaneState, QueuedPrompt,
};
use crate::teams;
use brehon_types::{PromptDeliveryMode, PromptId, RuntimeCommandKind, RuntimePolicyDecision};
use std::path::Path;
use std::time::Instant;
use tokio::sync::mpsc;

use super::Mux;
use super::format::{ensure_isolated_cwd_is_not_shared_root, prompt_delivery_notice};
use super::types::{
    AsyncGatewayPromptDeliveryError, AsyncGatewayPromptDispatch, GATEWAY_PROMPT_RETRY_DELAY,
    MuxEvent, PTY_INK_PROMPT_QUIET_THRESHOLD, PTY_STARTUP_PROMPT_DELAY_SECS, PromptDeliveryAttempt,
    STARTUP_PROMPT_STAGGER_MILLIS, SUPERVISOR_INBOX_ESCALATION_DELAY,
    SUPERVISOR_INBOX_ESCALATION_QUIET_THRESHOLD, SUPERVISOR_INBOX_RECOVERY_COOLDOWN,
    TEAMS_NUDGE_QUIET_THRESHOLD,
};

pub(super) fn gateway_env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find_map(|(env_key, value)| (env_key == key).then_some(value.as_str()))
}

pub(super) fn is_opencode_acp_spawn(spawn_config: &GatewaySpawnConfig) -> bool {
    spawn_config.protocol == brehon_acp::GatewayProtocol::AcpStdio
        && spawn_config.command.as_deref() == Some("opencode")
        && matches!(spawn_config.args.first().map(String::as_str), Some("acp"))
}

pub(super) fn is_opencode_model_config_spawn(spawn_config: &GatewaySpawnConfig) -> bool {
    is_opencode_acp_spawn(spawn_config)
        || spawn_config.protocol == brehon_acp::GatewayProtocol::OpenCodeServer
}

pub(super) fn opencode_model_candidates(spawn_config: &GatewaySpawnConfig) -> Vec<String> {
    let Some(model) = gateway_env_value(&spawn_config.env, "BREHON_AGENT_MODEL")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    if spawn_config.protocol != brehon_acp::GatewayProtocol::OpenCodeServer
        && let Some(reasoning_effort) =
            gateway_env_value(&spawn_config.env, "BREHON_REASONING_EFFORT")
                .map(str::trim)
                .filter(|value| !value.is_empty())
    {
        candidates.push(format!("{model}/{reasoning_effort}"));
    }
    candidates.push(model.to_string());
    candidates
}

impl Mux {
    fn pane_generation_for_observability(&self, pane_id: &str) -> Generation {
        self.panes
            .get(pane_id)
            .map(|pane| pane.current_generation())
            .unwrap_or_else(|| {
                tracing::warn!(
                    pane = %pane_id,
                    "Missing pane while capturing generation; defaulting generation to 0"
                );
                Generation::default()
            })
    }

    fn pane_death_reason(&self, pane_id: &str) -> Option<DeathReason> {
        self.panes
            .get(pane_id)
            .and_then(|pane| match pane.pane_state() {
                Some(PaneState::Dead { reason, .. }) => Some(reason.clone()),
                _ => None,
            })
    }

    fn new_prompt_id() -> PromptId {
        PromptId::new(uuid::Uuid::new_v4().to_string())
    }

    fn pane_prompt_backlog(&self, pane_id: &str) -> usize {
        self.panes
            .get(pane_id)
            .map(|pane| pane.delayed_prompt_count())
            .unwrap_or(0)
    }

    fn pane_prompt_backlog_with_live_turn(&self, pane_id: &str) -> usize {
        let backlog = self.pane_prompt_backlog(pane_id);
        if self.pane_has_live_gateway_turn(pane_id) {
            backlog.saturating_add(1)
        } else {
            backlog
        }
    }

    fn teams_inbox_write_not_before(&self, pane_id: &str) -> Option<Instant> {
        let pane = self.panes.get(pane_id)?;
        if !self.pane_uses_teams(pane) || pane.pending_inbox_nudge() {
            return None;
        }
        pane.inbox_nudge_not_before()
    }

    fn claude_prompt_state_for_pane(&self, pane_id: &str) -> ClaudePromptState {
        if self.is_panesmith_managed(pane_id)
            && let Some(prompt_state) = self.panesmith_claude_prompt_state(pane_id)
        {
            return prompt_state;
        }
        self.panes
            .get(pane_id)
            .map(|pane| pane.claude_prompt_state())
            .unwrap_or(ClaudePromptState::None)
    }

    fn pane_ready_for_inbox_nudge(
        &self,
        pane_id: &str,
        pane: &crate::pane::Pane,
        now: Instant,
    ) -> bool {
        if !pane.is_panesmith_managed() {
            return pane.is_ready_for_inbox_nudge(now, TEAMS_NUDGE_QUIET_THRESHOLD);
        }

        if pane.is_focused() {
            return false;
        }

        if now.saturating_duration_since(pane.last_output_at()) <= TEAMS_NUDGE_QUIET_THRESHOLD {
            return false;
        }

        if pane
            .inbox_nudge_not_before()
            .is_some_and(|not_before| now < not_before)
        {
            return false;
        }

        if pane.cli_type().capabilities().supports_teams {
            self.claude_prompt_state_for_pane(pane_id) == ClaudePromptState::Empty
        } else {
            true
        }
    }

    fn next_delayed_prompt_inject_after(&self, pane_id: &str, now: Instant) -> Instant {
        if self
            .panes
            .get(pane_id)
            .is_some_and(|pane| pane.is_gateway_backed())
            && self.pane_has_live_gateway_turn(pane_id)
        {
            return now + GATEWAY_PROMPT_RETRY_DELAY;
        }

        if let Some(not_before) = self.teams_inbox_write_not_before(pane_id)
            && now < not_before
        {
            return not_before;
        }

        if self.panes.get(pane_id).is_some_and(|pane| {
            !pane.is_ready_for_ink_prompt_injection(now, PTY_INK_PROMPT_QUIET_THRESHOLD)
        }) {
            return now + PTY_INK_PROMPT_QUIET_THRESHOLD;
        }

        now + GATEWAY_PROMPT_RETRY_DELAY
    }

    fn rejection_error(pane_id: &str, reason: &DeathReason) -> Error {
        Error::pty(format!(
            "Prompt delivery rejected for {pane_id}: {reason:?}"
        ))
    }

    async fn inject_unchecked(&self, pane_id: &str, prompt: &str) -> Result<()> {
        let pane = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.inject_prompt(prompt).await
    }

    /// Inject a prompt into a specific pane
    pub async fn inject(&mut self, pane_id: &str, prompt: &str) -> Result<()> {
        if !self.panes.contains_key(pane_id) {
            return Err(Error::pane_not_found(pane_id));
        }
        let prompt_id = Self::new_prompt_id();
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::SendPrompt {
                prompt_id: prompt_id.to_string(),
                text: prompt.to_string(),
                from: None,
                delivery: PromptDeliveryMode::Direct,
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("direct prompt injection", &decision) {
            return Err(err);
        }
        if let Some(outcome) = self
            .send_panesmith_prompt_transaction(pane_id, prompt)
            .await?
        {
            super::panesmith::ensure_panesmith_mux_outcome("direct prompt injection", &outcome)?;
            return Ok(());
        }

        self.inject_unchecked(pane_id, prompt).await
    }

    /// Inject a prompt into the focused pane
    pub async fn inject_focused(&mut self, prompt: &str) -> Result<()> {
        let pane_id = self
            .focused_id()
            .ok_or_else(|| Error::pty("No focused pane"))?
            .to_string();
        self.inject(&pane_id, prompt).await
    }

    /// Inject a prompt into all workers
    pub async fn inject_all_workers(&mut self, prompt: &str) -> Result<()> {
        let pane_ids: Vec<String> = self.workers().map(|pane| pane.id().to_string()).collect();
        let prompt_id = Self::new_prompt_id();
        let command = self.runtime_command_for_session(RuntimeCommandKind::BroadcastPrompt {
            prompt_id: prompt_id.to_string(),
            text: prompt.to_string(),
            pane_ids: pane_ids.clone(),
        });
        let context = self.runtime_policy_context_for_broadcast(pane_ids.len());
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("worker prompt broadcast", &decision) {
            return Err(err);
        }
        for pane_id in pane_ids {
            if let Some(outcome) = self
                .send_panesmith_prompt_transaction(&pane_id, prompt)
                .await?
            {
                super::panesmith::ensure_panesmith_mux_outcome(
                    "worker prompt broadcast",
                    &outcome,
                )?;
            } else {
                self.inject_unchecked(&pane_id, prompt).await?;
            }
        }
        Ok(())
    }

    /// Inject a prompt into the supervisor
    pub async fn inject_supervisor(&mut self, prompt: &str) -> Result<()> {
        let pane_id = self
            .supervisor()
            .map(|pane| pane.id().to_string())
            .ok_or_else(|| Error::pane_not_found("supervisor"))?;
        self.inject(&pane_id, prompt).await
    }

    /// Nudge a pane to check its Teams inbox (no visible text).
    ///
    /// Sends a plain Enter to trigger a new turn. The agent reads its
    /// inbox at turn start, so no message text is needed in the PTY.
    pub async fn nudge_inbox(&mut self, pane_id: &str) -> Result<()> {
        if !self.panes.contains_key(pane_id) {
            return Err(Error::pane_not_found(pane_id));
        }
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::SendTerminalInput {
                bytes: b"\r".to_vec(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("inbox nudge", &decision) {
            return Err(err);
        }

        if let Some(outcome) = self.send_panesmith_input_transaction(
            pane_id,
            super::panesmith::panesmith_enter_transaction(),
        )? {
            super::panesmith::ensure_panesmith_mux_outcome("inbox nudge", &outcome)?;
            return Ok(());
        }

        let pane = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.nudge_inbox().await
    }

    /// Send input to the focused pane
    pub async fn send_input(&mut self, data: &[u8]) -> Result<()> {
        let pane_id = self
            .focused_id()
            .ok_or_else(|| Error::pty("No focused pane"))?
            .to_string();
        self.send_input_to(&pane_id, data).await
    }

    /// Interrupt the focused pane
    pub async fn interrupt_focused(&mut self) -> Result<()> {
        let pane_id = self
            .focused_id()
            .ok_or_else(|| Error::pty("No focused pane"))?
            .to_string();
        self.interrupt(&pane_id).await
    }

    /// Interrupt a specific pane by ID
    pub async fn interrupt(&mut self, pane_id: &str) -> Result<()> {
        if !self.panes.contains_key(pane_id) {
            return Err(Error::pane_not_found(pane_id));
        }
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::Interrupt {
                reason: "manual interrupt".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("interrupt", &decision) {
            return Err(err);
        }

        if let Some(outcome) = self
            .send_panesmith_input_transaction(pane_id, panesmith::InputTransaction::interrupt())?
        {
            super::panesmith::ensure_panesmith_mux_outcome("interrupt", &outcome)?;
            return Ok(());
        }

        let pane = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.interrupt().await
    }

    /// Spawn all gateway-backed panes that have not started yet.
    ///
    /// Some ACP agents bootstrap themselves from CLI arguments or MCP server
    /// instructions rather than waiting for an external prompt. Those panes
    /// still need a live gateway session before they can register, so startup
    /// must explicitly initialize them instead of waiting for `deliver_prompt()`.
    pub async fn bootstrap_gateway_panes(&mut self) {
        self.bootstrap_gateway_panes_with_progress(|_| {}).await;
    }

    pub async fn bootstrap_gateway_panes_with_progress<F>(&mut self, mut report: F)
    where
        F: FnMut(String),
    {
        let pane_ids: Vec<String> = self
            .panes
            .iter()
            .filter(|(pane_id, pane)| {
                pane.is_gateway_backed()
                    && pane.gateway_session_id().is_none()
                    && !super::stability::agent_is_marked_unavailable(pane_id)
            })
            .map(|(pane_id, _)| pane_id.clone())
            .collect();

        for pane_id in pane_ids {
            report(format!("Bootstrapping agent session for {pane_id}"));
            if let Err(err) = self.ensure_gateway_session(&pane_id).await {
                report(format!("Agent session failed for {pane_id}: {err}"));
                tracing::warn!(
                    pane = %pane_id,
                    error = %err,
                    "Failed to bootstrap ACP gateway pane"
                );
            } else {
                report(format!("Agent session ready for {pane_id}"));
            }
        }
    }

    /// Send input to a specific pane.
    pub async fn send_input_to(&mut self, pane_id: &str, data: &[u8]) -> Result<()> {
        if !self.panes.contains_key(pane_id) {
            return Err(Error::pane_not_found(pane_id));
        }
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::SendTerminalInput {
                bytes: data.to_vec(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("terminal input", &decision) {
            return Err(err);
        }

        let clear_pending_inbox_nudge = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?
            .should_clear_pending_inbox_nudge_on_manual_input(data);

        let is_gateway = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?
            .is_gateway_backed();

        if is_gateway {
            return self.send_input_via_gateway(pane_id, data).await;
        }

        if self.send_panesmith_input_bytes(pane_id, data)? {
            if clear_pending_inbox_nudge && let Some(pane) = self.panes.get_mut(pane_id) {
                pane.set_pending_inbox_nudge(false);
                tracing::info!(
                    pane = %pane_id,
                    "Cleared Teams inbox nudge after manual Enter at empty prompt"
                );
            }
            return Ok(());
        }

        let pane = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.write(data).await?;
        if clear_pending_inbox_nudge && let Some(pane) = self.panes.get_mut(pane_id) {
            pane.set_pending_inbox_nudge(false);
            tracing::info!(
                pane = %pane_id,
                "Cleared Teams inbox nudge after manual Enter at empty prompt"
            );
        }
        Ok(())
    }

    /// Dispatch input to the focused pane without blocking the caller.
    pub fn dispatch_input_focused(&mut self, rt: &tokio::runtime::Handle, data: Vec<u8>) {
        let Some(pane_id) = self.focused_id().map(|s| s.to_string()) else {
            return;
        };
        self.dispatch_input_to(rt, &pane_id, data);
    }

    /// Dispatch input to a specific pane without blocking the caller.
    pub fn dispatch_input_to(&mut self, rt: &tokio::runtime::Handle, pane_id: &str, data: Vec<u8>) {
        let Some(pane) = self.panes.get(pane_id) else {
            return;
        };

        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::SendTerminalInput {
                bytes: data.clone(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy_immediate(command, context);
        if let Some(err) = Self::policy_decision_error("terminal input dispatch", &decision) {
            tracing::warn!(pane = %pane_id, error = %err, "Dropped input dispatch by policy");
            return;
        }

        let clear_pending_inbox_nudge =
            pane.should_clear_pending_inbox_nudge_on_manual_input(&data);

        if clear_pending_inbox_nudge && let Some(pane) = self.panes.get_mut(pane_id) {
            pane.set_pending_inbox_nudge(false);
        }

        let is_gateway = self
            .panes
            .get(pane_id)
            .map(|pane| pane.is_gateway_backed())
            .unwrap_or(false);

        if is_gateway {
            let Some(gateway) = self.gateway.clone() else {
                return;
            };
            let Some(terminal_id) = self
                .panes
                .get(pane_id)
                .and_then(|pane| pane.gateway_terminal_id())
                .map(|s| s.to_string())
            else {
                tracing::debug!(pane = %pane_id, "Gateway terminal not ready for input dispatch");
                return;
            };
            let pane_id = pane_id.to_string();
            rt.spawn(async move {
                let terminal_id = brehon_types::TerminalId::new(terminal_id);
                if let Err(err) = brehon_ports::AgentGateway::send_terminal_input(&gateway, &terminal_id, data).await {
                    tracing::warn!(pane = %pane_id, error = %err, "Non-blocking gateway input dispatch failed");
                }
            });
        } else if self
            .send_panesmith_input_bytes(pane_id, &data)
            .unwrap_or_else(|err| {
                tracing::warn!(
                    pane = %pane_id,
                    error = %err,
                    "Panesmith input dispatch failed"
                );
                false
            })
        {
            // Input was routed through Panesmith's explicit raw-byte escape hatch.
        } else if let Some(writer) = self
            .panes
            .get(pane_id)
            .and_then(|pane| pane.pty_writer_handle())
        {
            let mut w = writer.lock().expect("PTY writer mutex poisoned");
            if let Err(err) = w.write_all(&data) {
                tracing::warn!(pane = %pane_id, error = %err, "PTY input dispatch failed");
            }
            if let Err(err) = w.flush() {
                tracing::warn!(pane = %pane_id, error = %err, "PTY input flush failed");
            }
        }
    }

    /// Dispatch a Teams inbox nudge without blocking the caller.
    ///
    /// `_rt` is kept on the signature for symmetry with the other
    /// `dispatch_*` methods. The previous implementation used
    /// `rt.spawn_blocking` to acquire a `tokio::sync::Mutex` lock; after
    /// F2 the writer is a `std::sync::Mutex` whose critical section is a
    /// single sync `write_all(b"\r")` + `flush`. Dispatching a tokio
    /// blocking task costs ~10–100 µs of scheduling overhead just to do
    /// ~1 µs of byte-writing — inline is both faster and simpler.
    pub fn dispatch_nudge_inbox(&mut self, _rt: &tokio::runtime::Handle, pane_id: &str) {
        let Some(pane) = self.panes.get(pane_id) else {
            return;
        };
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::SendTerminalInput {
                bytes: b"\r".to_vec(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy_immediate(command, context);
        if let Some(err) = Self::policy_decision_error("inbox nudge", &decision) {
            tracing::warn!(pane = %pane_id, error = %err, "Dropped inbox nudge by policy");
            return;
        }
        if self.is_panesmith_managed(pane_id) {
            if let Err(err) = self
                .send_panesmith_input_transaction(
                    pane_id,
                    super::panesmith::panesmith_enter_transaction(),
                )
                .and_then(|outcome| {
                    let outcome = outcome.ok_or_else(|| Error::pane_not_found(pane_id))?;
                    super::panesmith::ensure_panesmith_mux_outcome("inbox nudge", &outcome)
                })
            {
                tracing::warn!(
                    pane = %pane_id,
                    error = %err,
                    "Panesmith inbox nudge failed; flush_pending_inbox_nudges will re-detect on the next cycle"
                );
                return;
            }
            if let Some(pane) = self.panes.get_mut(pane_id) {
                pane.set_pending_inbox_nudge(false);
            }
            return;
        }
        let Some(writer) = pane.pty_writer_handle() else {
            return;
        };
        // Clear flag first so flush_pending_inbox_nudges doesn't re-dispatch
        // while we're mid-write.
        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.set_pending_inbox_nudge(false);
        }
        let mut w = writer.lock().expect("PTY writer mutex poisoned");
        if let Err(err) = w.write_all(b"\r").and_then(|_| w.flush()) {
            tracing::warn!(
                pane = %pane_id,
                error = %err,
                "Inbox nudge write failed; flush_pending_inbox_nudges will re-detect on the next cycle"
            );
        }
    }

    /// Prepare an async gateway prompt delivery owned by the mux.
    ///
    /// This is the canonical non-blocking gateway path for delayed and durable
    /// queued prompts. It ensures the session exists before any async send is
    /// started, so callers do not need to duplicate gateway bootstrap logic.
    pub async fn begin_async_gateway_prompt_delivery(
        &mut self,
        rt: &tokio::runtime::Handle,
        pane_id: &str,
        prompt: &str,
    ) -> Result<AsyncGatewayPromptDispatch> {
        if let Some(reason) = self.pane_death_reason(pane_id) {
            return Err(Self::rejection_error(pane_id, &reason));
        }
        if super::stability::agent_is_marked_unavailable(pane_id) {
            return Err(Error::pty(format!(
                "Agent {pane_id} is quarantined unavailable for this run"
            )));
        }

        let Some(pane) = self.panes.get(pane_id) else {
            return Err(Error::pane_not_found(pane_id));
        };
        if !pane.is_gateway_backed() {
            return Err(Error::pty(format!("Pane {pane_id} is not gateway-backed")));
        }

        let prompt_id = Self::new_prompt_id();
        let generation = self.pane_generation_for_observability(pane_id);
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::SendPrompt {
                prompt_id: prompt_id.to_string(),
                text: prompt.to_string(),
                from: None,
                delivery: PromptDeliveryMode::Enqueue,
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        match self.evaluate_runtime_policy(command, context).await {
            RuntimePolicyDecision::Allow => {}
            RuntimePolicyDecision::Defer { .. } => {
                return Ok(AsyncGatewayPromptDispatch::Queued {
                    prompt_id,
                    ahead_of: self.pane_prompt_backlog(pane_id).saturating_add(1),
                });
            }
            decision => {
                let reason = Self::policy_rejection_reason(&decision);
                return Err(Self::rejection_error(pane_id, &reason));
            }
        }
        if self.pane_has_live_gateway_turn(pane_id) {
            return Ok(AsyncGatewayPromptDispatch::Queued {
                prompt_id,
                ahead_of: self.pane_prompt_backlog_with_live_turn(pane_id),
            });
        }

        self.ensure_gateway_session(pane_id).await?;

        let session_id = self
            .panes
            .get(pane_id)
            .and_then(|pane| pane.gateway_session_id())
            .map(brehon_types::SessionId::new)
            .ok_or_else(|| {
                Error::pty(format!(
                    "Gateway session missing for {} after session bootstrap",
                    pane_id
                ))
            })?;
        let gateway = self
            .gateway
            .as_ref()
            .cloned()
            .ok_or_else(|| Error::pty("Agent gateway not configured"))?;

        let prompt_turn = brehon_types::PromptTurn {
            prompt_id: prompt_id.clone(),
            content: prompt.to_string(),
            kind: brehon_types::MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };
        let queued_prompt_id = prompt_id.clone();

        Ok(AsyncGatewayPromptDispatch::Started(rt.spawn(async move {
            match brehon_ports::AgentGateway::send_prompt(&gateway, &session_id, prompt_turn).await
            {
                Ok(_) => Ok(PromptDeliveryAttempt::Delivered {
                    prompt_id,
                    generation,
                }),
                Err(err) => {
                    let err_text = err.to_string();
                    if Self::is_busy_gateway_delivery_error(&err_text) {
                        Ok(PromptDeliveryAttempt::Queued {
                            prompt_id: queued_prompt_id,
                            ahead_of: 1,
                        })
                    } else {
                        Err(AsyncGatewayPromptDeliveryError { error: err_text })
                    }
                }
            }
        })))
    }

    /// Dispatch prompt delivery without blocking the caller.
    ///
    /// This is a fire-and-forget variant of `deliver_prompt` for use in the
    /// TUI hot path. If the pane is not ready (Teams settle deadline, Ink
    /// prompt not ready, or gateway session missing), the prompt is internally
    /// re-queued via `queue_delayed_prompt` for later retry.
    pub fn dispatch_deliver_prompt(
        &mut self,
        rt: &tokio::runtime::Handle,
        pane_id: &str,
        prompt: String,
        from: Option<String>,
    ) {
        if let Some(reason) = self.pane_death_reason(pane_id) {
            tracing::warn!(
                pane = %pane_id,
                reason = ?reason,
                "Dropped prompt dispatch for dead pane"
            );
            return;
        }
        if super::stability::agent_is_marked_unavailable(pane_id) {
            tracing::warn!(
                pane = %pane_id,
                "Dropped prompt dispatch for quarantined agent"
            );
            return;
        }

        let Some(pane) = self.panes.get(pane_id) else {
            return;
        };
        let control_plane = pane.cli_type().control_plane();
        let injector = pane.injector_handle();
        let pane_is_gateway_backed = pane.is_gateway_backed();
        let pane_uses_teams = self.pane_uses_teams(pane);
        let teams_inbox_write_not_before = if pane_uses_teams && !pane.pending_inbox_nudge() {
            pane.inbox_nudge_not_before()
        } else {
            None
        };
        let pty_prompt_ready =
            pane.is_ready_for_ink_prompt_injection(Instant::now(), PTY_INK_PROMPT_QUIET_THRESHOLD);
        let gateway_policy_owned_by_async_dispatch = matches!(
            control_plane,
            HarnessControlPlane::Acp
                | HarnessControlPlane::AcpSidecar
                | HarnessControlPlane::OpenAiCompatible
        ) && pane_is_gateway_backed;

        if !gateway_policy_owned_by_async_dispatch {
            let prompt_id = Self::new_prompt_id();
            let command = self.runtime_command_for_pane(
                pane_id,
                RuntimeCommandKind::SendPrompt {
                    prompt_id: prompt_id.to_string(),
                    text: prompt.clone(),
                    from: from.clone(),
                    delivery: PromptDeliveryMode::Enqueue,
                },
            );
            let context = self.runtime_policy_context_for_pane(pane_id);
            match self.evaluate_runtime_policy_immediate(command, context) {
                RuntimePolicyDecision::Allow => {}
                RuntimePolicyDecision::Defer { .. } => {
                    let now = Instant::now();
                    let inject_after = self.next_delayed_prompt_inject_after(pane_id, now);
                    match self.queue_delayed_prompt(
                        pane_id,
                        prompt,
                        from,
                        inject_after,
                        Some(prompt_id.clone()),
                    ) {
                        PromptDeliveryAttempt::Queued { ahead_of, .. } => {
                            tracing::info!(
                                pane = %pane_id,
                                prompt_id = %prompt_id,
                                ahead_of,
                                deliver_after_ms = %inject_after.saturating_duration_since(now).as_millis(),
                                "Queued prompt dispatch after policy defer"
                            );
                        }
                        PromptDeliveryAttempt::AlreadyPresent { position, .. } => {
                            tracing::debug!(
                                pane = %pane_id,
                                prompt_id = %prompt_id,
                                position = %position,
                                "Prompt dispatch already queued after policy defer"
                            );
                        }
                        PromptDeliveryAttempt::Rejected { reason } => {
                            tracing::warn!(
                                pane = %pane_id,
                                prompt_id = %prompt_id,
                                reason = ?reason,
                                "Rejected prompt queueing after policy defer"
                            );
                        }
                        PromptDeliveryAttempt::Delivered { .. } => {}
                    }
                    return;
                }
                decision => {
                    tracing::warn!(
                        pane = %pane_id,
                        reason = ?Self::policy_rejection_reason(&decision),
                        "Dropped prompt dispatch by policy"
                    );
                    return;
                }
            }
        }

        match control_plane {
            HarnessControlPlane::Acp
            | HarnessControlPlane::AcpSidecar
            | HarnessControlPlane::OpenAiCompatible => {
                if pane_is_gateway_backed {
                    match rt
                        .block_on(self.begin_async_gateway_prompt_delivery(rt, pane_id, &prompt))
                    {
                        Ok(AsyncGatewayPromptDispatch::Started(handle)) => {
                            let generation = self.pane_generation_for_observability(pane_id);
                            let pane_id = pane_id.to_string();
                            let event_tx = self.event_tx.clone();
                            rt.spawn(async move {
                                let result = match handle.await {
                                    Ok(result) => result,
                                    Err(err) => Err(AsyncGatewayPromptDeliveryError {
                                        error: err.to_string(),
                                    }),
                                };
                                let _ = event_tx
                                    .send(MuxEvent::AsyncGatewayPromptDeliveryCompleted {
                                        pane_id,
                                        prompt,
                                        from,
                                        generation,
                                        result,
                                    })
                                    .await;
                            });
                        }
                        Ok(AsyncGatewayPromptDispatch::Queued {
                            prompt_id,
                            ahead_of,
                        }) => {
                            let now = Instant::now();
                            let inject_after = now + GATEWAY_PROMPT_RETRY_DELAY;
                            match self.queue_delayed_prompt(
                                pane_id,
                                prompt,
                                from,
                                inject_after,
                                Some(prompt_id.clone()),
                            ) {
                                PromptDeliveryAttempt::Queued { .. } => {
                                    tracing::info!(
                                        pane = %pane_id,
                                        prompt_id = %prompt_id,
                                        ahead_of,
                                        deliver_after_ms = %inject_after.saturating_duration_since(now).as_millis(),
                                        "Queued non-blocking gateway prompt dispatch while the agent turn is active"
                                    );
                                }
                                PromptDeliveryAttempt::AlreadyPresent { position, .. } => {
                                    tracing::debug!(
                                        pane = %pane_id,
                                        prompt_id = %prompt_id,
                                        position = %position,
                                        "Prompt already queued for non-blocking gateway dispatch"
                                    );
                                }
                                PromptDeliveryAttempt::Rejected { reason } => {
                                    tracing::warn!(
                                        pane = %pane_id,
                                        prompt_id = %prompt_id,
                                        reason = ?reason,
                                        "Rejected non-blocking gateway prompt queueing"
                                    );
                                }
                                PromptDeliveryAttempt::Delivered { .. } => {}
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                pane = %pane_id,
                                error = %err,
                                "Failed to start non-blocking gateway prompt dispatch"
                            );
                        }
                    }
                    return;
                }
                // Non-gateway ACP pane: fall through to PTY injection.
            }
            HarnessControlPlane::NativeHooks => {
                if let Some(ref teams) = self.teams {
                    if !pane_uses_teams {
                        // Fall through to PTY injection for NativeHooks panes
                        // that do not actually support Teams.
                    } else {
                        let sender = from
                            .clone()
                            .unwrap_or_else(|| teams::AUTOMATION_AGENT_NAME.to_string());
                        if let Some(inject_after) = teams_inbox_write_not_before
                            .filter(|inject_after| Instant::now() < *inject_after)
                        {
                            match self.queue_delayed_prompt(
                                pane_id,
                                prompt,
                                from,
                                inject_after,
                                None,
                            ) {
                                PromptDeliveryAttempt::Queued {
                                    prompt_id,
                                    ahead_of,
                                } => {
                                    tracing::info!(
                                        pane = %pane_id,
                                        prompt_id = %prompt_id,
                                        ahead_of,
                                        deliver_after_ms = %inject_after.saturating_duration_since(Instant::now()).as_millis(),
                                        "Queued Teams inbox delivery until Claude settle deadline"
                                    );
                                }
                                PromptDeliveryAttempt::AlreadyPresent {
                                    prompt_id,
                                    position,
                                } => {
                                    tracing::debug!(
                                        pane = %pane_id,
                                        prompt_id = %prompt_id,
                                        position = %position,
                                        "Teams inbox prompt already queued for delivery"
                                    );
                                }
                                PromptDeliveryAttempt::Rejected { reason } => {
                                    tracing::warn!(
                                        pane = %pane_id,
                                        reason = ?reason,
                                        "Rejected Teams inbox prompt queueing"
                                    );
                                }
                                PromptDeliveryAttempt::Delivered { .. } => {}
                            }
                            return;
                        }
                        if let Some(pane) = self.panes.get_mut(pane_id) {
                            pane.set_pending_inbox_nudge(true);
                            if let Err(err) = pane.append_inbox_queue_notice(&sender) {
                                tracing::warn!(
                                    pane = %pane_id,
                                    error = %err,
                                    "Failed to append Teams inbox queue notice"
                                );
                            }
                        }
                        let teams = teams.clone();
                        let pane_id = pane_id.to_string();
                        let team = teams.team_name().to_string();
                        let prompt_id = Self::new_prompt_id();
                        let generation = self.pane_generation_for_observability(&pane_id);
                        let event_tx = self.event_tx.clone();
                        rt.spawn_blocking(move || {
                            let result = teams
                                .write_to_inbox(&pane_id, &sender, &prompt, None)
                                .map(|_| PromptDeliveryAttempt::Delivered {
                                    prompt_id,
                                    generation,
                                })
                                .map_err(|err| AsyncGatewayPromptDeliveryError {
                                    error: format!("Teams inbox write failed: {err}"),
                                });
                            let _ = event_tx.blocking_send(
                                MuxEvent::AsyncTeamsPromptDeliveryCompleted {
                                    pane_id,
                                    team,
                                    generation,
                                    result,
                                },
                            );
                        });
                        return;
                    }
                }
            }
            HarnessControlPlane::PtyInjection | HarnessControlPlane::OneShot => {}
        }

        if self.is_panesmith_managed(pane_id) {
            match rt.block_on(self.send_panesmith_prompt_transaction(pane_id, &prompt)) {
                Ok(Some(outcome)) => {
                    if outcome.is_success() {
                        if let Some(pane) = self.panes.get_mut(pane_id)
                            && pane.is_agy_or_opencode_supervisor()
                        {
                            pane.last_prompt_delivery_attempt = Some(Instant::now());
                        }
                        return;
                    }
                    let error = super::panesmith::ensure_panesmith_mux_outcome(
                        "prompt transaction",
                        &outcome,
                    )
                    .err();
                    tracing::warn!(
                        pane = %pane_id,
                        error = ?error,
                        outcome = ?outcome,
                        "Panesmith prompt transaction failed"
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        pane = %pane_id,
                        error = %err,
                        "Panesmith prompt transaction failed"
                    );
                }
            }
        } else if let Some(injector) = injector {
            if !pty_prompt_ready {
                let inject_after = Instant::now() + PTY_INK_PROMPT_QUIET_THRESHOLD;
                match self.queue_delayed_prompt(pane_id, prompt, from, inject_after, None) {
                    PromptDeliveryAttempt::Queued {
                        prompt_id,
                        ahead_of,
                    } => {
                        tracing::info!(
                            pane = %pane_id,
                            prompt_id = %prompt_id,
                            ahead_of,
                            deliver_after_ms = %inject_after.saturating_duration_since(Instant::now()).as_millis(),
                            "Queued PTY prompt delivery until Ink prompt is ready"
                        );
                    }
                    PromptDeliveryAttempt::AlreadyPresent {
                        prompt_id,
                        position,
                    } => {
                        tracing::debug!(
                            pane = %pane_id,
                            prompt_id = %prompt_id,
                            position = %position,
                            "PTY prompt already queued for delayed delivery"
                        );
                    }
                    PromptDeliveryAttempt::Rejected { reason } => {
                        tracing::warn!(
                            pane = %pane_id,
                            reason = ?reason,
                            "Rejected PTY prompt queueing"
                        );
                    }
                    PromptDeliveryAttempt::Delivered { .. } => {}
                }
                return;
            }
            let pane_id = pane_id.to_string();
            if let Some(pane) = self.panes.get_mut(&pane_id)
                && pane.is_agy_or_opencode_supervisor()
            {
                pane.last_prompt_delivery_attempt = Some(Instant::now());
            }
            rt.spawn(async move {
                if let Err(err) = injector.inject_prompt(&prompt).await {
                    tracing::warn!(pane = %pane_id, error = %err, "Non-blocking PTY prompt injection failed");
                }
            });
        }
    }

    /// Returns true if the given pane should use Teams inbox delivery.
    ///
    /// Checks both `capabilities().supports_teams` and the presence of an
    /// active TeamsManager so that custom agents with `NativeHooks` control
    /// plane but `supports_teams == false` do not get routed to Teams.
    fn pane_uses_teams(&self, pane: &crate::pane::Pane) -> bool {
        self.teams.is_some() && pane.cli_type().capabilities().supports_teams
    }

    fn pane_unavailable_reason(&self, pane_id: &str) -> Option<DeathReason> {
        if super::stability::agent_is_marked_unavailable(pane_id) {
            Some(DeathReason::Quarantined(format!(
                "Agent {pane_id} is quarantined unavailable for this run"
            )))
        } else {
            None
        }
    }

    /// Set the Teams manager for native Claude Code inbox delivery.
    pub fn set_teams(&mut self, teams: crate::teams::TeamsManager) {
        self.teams = Some(teams);
    }

    /// Get a reference to the Teams manager.
    pub fn teams(&self) -> Option<&crate::teams::TeamsManager> {
        self.teams.as_ref()
    }

    pub(super) fn queue_delayed_prompt(
        &mut self,
        pane_id: &str,
        prompt: String,
        from: Option<String>,
        inject_after: Instant,
        prompt_id: Option<PromptId>,
    ) -> PromptDeliveryAttempt {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return PromptDeliveryAttempt::Rejected {
                reason: DeathReason::SessionDropped,
            };
        };

        if let Some(PaneState::Dead { reason, .. }) = pane.pane_state() {
            return PromptDeliveryAttempt::Rejected {
                reason: reason.clone(),
            };
        }

        if let Some(prompt_id) = prompt_id.as_ref()
            && let Some(position) = pane.delayed_prompt_position_by_id(prompt_id)
        {
            return PromptDeliveryAttempt::AlreadyPresent {
                prompt_id: prompt_id.clone(),
                position,
            };
        }

        if let Some((existing_prompt_id, position)) =
            pane.delayed_prompt_position_by_content(&prompt, from.as_deref())
        {
            return PromptDeliveryAttempt::AlreadyPresent {
                prompt_id: existing_prompt_id,
                position,
            };
        }

        let ahead_of = pane.delayed_prompt_count();
        let prompt_id = prompt_id.unwrap_or_else(Self::new_prompt_id);
        let generation = pane.current_generation();
        let queued = QueuedPrompt {
            prompt_id: prompt_id.clone(),
            prompt,
            from,
            inject_after,
            generation,
        };
        if pane.enqueue_delayed_prompt(queued, DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP) {
            PromptDeliveryAttempt::Queued {
                prompt_id,
                ahead_of,
            }
        } else {
            tracing::warn!(
                pane = %pane_id,
                generation = generation.0,
                ahead_of,
                "Dropped delayed prompt because per-pane queue depth was exceeded"
            );
            PromptDeliveryAttempt::Rejected {
                reason: DeathReason::Quarantined("prompt queue depth exceeded".to_string()),
            }
        }
    }

    /// Number of prompts currently buffered inside mux for delayed delivery.
    pub fn pending_delayed_prompt_count(&self) -> usize {
        self.panes
            .values()
            .map(|pane| pane.delayed_prompt_count())
            .sum()
    }

    async fn attempt_acp_delivery(
        &mut self,
        pane_id: &str,
        prompt: &str,
        from: Option<&str>,
        prompt_id: PromptId,
        generation: Generation,
    ) -> Result<PromptDeliveryAttempt> {
        let pane = self.panes.get(pane_id);
        if pane.map(|p| p.is_gateway_backed()).unwrap_or(false) {
            self.deliver_via_gateway_once(pane_id, prompt, from, prompt_id, generation)
                .await
        } else {
            self.attempt_pty_delivery(pane_id, prompt, from, prompt_id, generation)
                .await
        }
    }

    async fn attempt_native_hooks_delivery(
        &mut self,
        pane_id: &str,
        prompt: &str,
        from: Option<&str>,
        prompt_id: PromptId,
        generation: Generation,
    ) -> Result<PromptDeliveryAttempt> {
        let teams = self.teams.clone();
        if let (Some(teams), Some(pane)) = (teams, self.panes.get(pane_id))
            && self.pane_uses_teams(pane)
        {
            return self
                .attempt_teams_delivery(pane_id, prompt, from, prompt_id, generation, &teams)
                .await;
        }
        self.attempt_pty_delivery(pane_id, prompt, from, prompt_id, generation)
            .await
    }

    async fn attempt_teams_delivery(
        &mut self,
        pane_id: &str,
        prompt: &str,
        from: Option<&str>,
        prompt_id: PromptId,
        generation: Generation,
        teams: &crate::teams::TeamsManager,
    ) -> Result<PromptDeliveryAttempt> {
        if let Some(inject_after) = self.teams_inbox_write_not_before(pane_id)
            && Instant::now() < inject_after
        {
            return Ok(PromptDeliveryAttempt::Queued {
                prompt_id,
                ahead_of: self.pane_prompt_backlog(pane_id).saturating_add(1),
            });
        }

        let sender = from.unwrap_or(teams::AUTOMATION_AGENT_NAME);
        if let Err(err) = teams.write_to_inbox(pane_id, sender, prompt, None) {
            let error = format!("Teams inbox write failed: {err}");
            Self::log_teams_inbox_delivery_failure(teams.team_name(), pane_id, &error);
            return Ok(PromptDeliveryAttempt::Rejected {
                reason: DeathReason::Quarantined(error),
            });
        }
        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.set_pending_inbox_nudge(true);
            if let Err(err) = pane.append_inbox_queue_notice(sender) {
                tracing::warn!(
                    pane = %pane_id,
                    error = %err,
                    "Failed to append Teams inbox queue notice"
                );
            }
        }
        tracing::info!(pane = %pane_id, "Delivered prompt via Teams inbox");
        Ok(PromptDeliveryAttempt::Delivered {
            prompt_id,
            generation,
        })
    }

    async fn attempt_pty_delivery(
        &mut self,
        pane_id: &str,
        prompt: &str,
        _from: Option<&str>,
        prompt_id: PromptId,
        generation: Generation,
    ) -> Result<PromptDeliveryAttempt> {
        if let Some(outcome) = self
            .send_panesmith_prompt_transaction(pane_id, prompt)
            .await?
        {
            super::panesmith::ensure_panesmith_mux_outcome("prompt transaction", &outcome)?;
            if let Some(pane) = self.panes.get_mut(pane_id)
                && pane.is_agy_or_opencode_supervisor()
            {
                pane.last_prompt_delivery_attempt = Some(Instant::now());
            }
            return Ok(PromptDeliveryAttempt::Delivered {
                prompt_id,
                generation,
            });
        }

        if self.panes.get(pane_id).is_some_and(|pane| {
            !pane.is_ready_for_ink_prompt_injection(Instant::now(), PTY_INK_PROMPT_QUIET_THRESHOLD)
        }) {
            return Ok(PromptDeliveryAttempt::Queued {
                prompt_id,
                ahead_of: self.pane_prompt_backlog(pane_id).saturating_add(1),
            });
        }

        self.inject_unchecked(pane_id, prompt).await?;
        if let Some(pane) = self.panes.get_mut(pane_id)
            && pane.is_agy_or_opencode_supervisor()
        {
            pane.last_prompt_delivery_attempt = Some(Instant::now());
        }
        Ok(PromptDeliveryAttempt::Delivered {
            prompt_id,
            generation,
        })
    }

    /// Attempt a single prompt delivery without mutating mux-owned retry state.
    ///
    /// Callers that already have a durable queue on disk should use this API so
    /// the queue remains the source of truth until the transport actually
    /// accepts the prompt.
    pub async fn attempt_prompt_delivery(
        &mut self,
        pane_id: &str,
        prompt: &str,
        from: Option<&str>,
    ) -> Result<PromptDeliveryAttempt> {
        if !self.panes.contains_key(pane_id) {
            return Err(Error::pane_not_found(pane_id));
        }
        if let Some((existing_prompt_id, position)) = self
            .panes
            .get(pane_id)
            .and_then(|pane| pane.delayed_prompt_position_by_content(prompt, from))
        {
            return Ok(PromptDeliveryAttempt::AlreadyPresent {
                prompt_id: existing_prompt_id,
                position,
            });
        }
        let prompt_id = Self::new_prompt_id();
        let generation = self.pane_generation_for_observability(pane_id);
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::SendPrompt {
                prompt_id: prompt_id.to_string(),
                text: prompt.to_string(),
                from: from.map(ToOwned::to_owned),
                delivery: PromptDeliveryMode::Enqueue,
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        match self.evaluate_runtime_policy(command, context).await {
            RuntimePolicyDecision::Allow => {}
            RuntimePolicyDecision::Defer { .. } => {
                return Ok(PromptDeliveryAttempt::Queued {
                    prompt_id,
                    ahead_of: self.pane_prompt_backlog(pane_id).saturating_add(1),
                });
            }
            decision => {
                return Ok(PromptDeliveryAttempt::Rejected {
                    reason: Self::policy_rejection_reason(&decision),
                });
            }
        }

        if let Some(reason) = self.pane_death_reason(pane_id) {
            return Ok(PromptDeliveryAttempt::Rejected { reason });
        }
        if let Some(reason) = self.pane_unavailable_reason(pane_id) {
            return Ok(PromptDeliveryAttempt::Rejected { reason });
        }

        let pane = self.panes.get(pane_id);
        let control_plane = pane
            .map(|p| p.cli_type().control_plane())
            .unwrap_or(HarnessControlPlane::PtyInjection);

        match control_plane {
            HarnessControlPlane::Acp
            | HarnessControlPlane::AcpSidecar
            | HarnessControlPlane::OpenAiCompatible => {
                self.attempt_acp_delivery(pane_id, prompt, from, prompt_id, generation)
                    .await
            }
            HarnessControlPlane::NativeHooks => {
                self.attempt_native_hooks_delivery(pane_id, prompt, from, prompt_id, generation)
                    .await
            }
            HarnessControlPlane::PtyInjection | HarnessControlPlane::OneShot => {
                self.attempt_pty_delivery(pane_id, prompt, from, prompt_id, generation)
                    .await
            }
        }
    }

    /// Deliver a prompt to an agent, routing through the appropriate transport.
    ///
    /// This is the canonical in-memory delivery path for startup prompts,
    /// nudges, and other transient messages. Durable queue consumers should use
    /// `attempt_prompt_delivery()` instead.
    pub async fn deliver_prompt(
        &mut self,
        pane_id: &str,
        prompt: &str,
        from: Option<&str>,
    ) -> Result<()> {
        match self.attempt_prompt_delivery(pane_id, prompt, from).await? {
            PromptDeliveryAttempt::Delivered { .. } => Ok(()),
            PromptDeliveryAttempt::Queued { prompt_id, .. } => {
                let now = Instant::now();
                let inject_after = self.next_delayed_prompt_inject_after(pane_id, now);
                match self.queue_delayed_prompt(
                    pane_id,
                    prompt.to_string(),
                    from.map(ToOwned::to_owned),
                    inject_after,
                    Some(prompt_id),
                ) {
                    PromptDeliveryAttempt::Queued {
                        prompt_id,
                        ahead_of,
                    } => {
                        tracing::info!(
                            pane = %pane_id,
                            prompt_id = %prompt_id,
                            ahead_of,
                            deliver_after_ms = %inject_after.saturating_duration_since(now).as_millis(),
                            "Queued prompt delivery into mux retry queue"
                        );
                        Ok(())
                    }
                    PromptDeliveryAttempt::AlreadyPresent {
                        prompt_id,
                        position,
                    } => {
                        tracing::debug!(
                            pane = %pane_id,
                            prompt_id = %prompt_id,
                            position = %position,
                            "Prompt already present in mux retry queue"
                        );
                        Ok(())
                    }
                    PromptDeliveryAttempt::Rejected { reason } => {
                        Err(Self::rejection_error(pane_id, &reason))
                    }
                    PromptDeliveryAttempt::Delivered { .. } => Ok(()),
                }
            }
            PromptDeliveryAttempt::Rejected { reason } => {
                Err(Self::rejection_error(pane_id, &reason))
            }
            PromptDeliveryAttempt::AlreadyPresent { .. } => Ok(()),
        }
    }

    /// Deliver a prompt to an ACP agent through the AgentGateway.
    ///
    /// Lazily spawns the gateway session on first delivery if needed.
    async fn deliver_via_gateway_once(
        &mut self,
        pane_id: &str,
        prompt: &str,
        from: Option<&str>,
        prompt_id: PromptId,
        _generation: Generation,
    ) -> Result<PromptDeliveryAttempt> {
        if self.pane_has_live_gateway_turn(pane_id) {
            return Ok(PromptDeliveryAttempt::Queued {
                prompt_id,
                ahead_of: self.pane_prompt_backlog_with_live_turn(pane_id),
            });
        }

        // Ensure gateway exists and spawn session if needed (while Arc refcount is 1)
        self.ensure_gateway_session(pane_id).await?;
        let generation = self.pane_generation_for_observability(pane_id);

        let session_id = {
            let pane = self
                .panes
                .get_mut(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;
            let session_id_str = pane.gateway_session_id().unwrap().to_string();
            brehon_types::SessionId::new(&session_id_str)
        };

        let prompt_turn = brehon_types::PromptTurn {
            prompt_id: prompt_id.clone(),
            content: prompt.to_string(),
            kind: brehon_types::MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };

        let gateway = self
            .gateway
            .as_ref()
            .ok_or_else(|| Error::pty("Agent gateway not configured"))?;

        match brehon_ports::AgentGateway::send_prompt(gateway, &session_id, prompt_turn).await {
            Ok(handle) => {
                super::stability::clear_agent_health_marker(pane_id);
                let delivered_at = Instant::now();
                let mut state_change = None;
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
                    pane.set_last_output_at(delivered_at);
                    pane.set_tool_executing(true);
                    pane.set_pane_busy(handle.prompt_id.clone(), generation, delivered_at);
                    let notice = prompt_delivery_notice(prompt, from);
                    let _ = pane.append_output(notice.as_bytes());
                    state_change =
                        Self::runtime_state_change(previous, pane.pane_state(), "prompt delivered");
                }
                if let Some((previous, current, reason, blocked)) = state_change {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        generation,
                        previous,
                        current,
                        Some(reason),
                        blocked,
                    );
                }
                tracing::info!(
                    pane = %pane_id,
                    prompt_id = %handle.prompt_id,
                    "Prompt delivered via agent gateway"
                );
                Ok(PromptDeliveryAttempt::Delivered {
                    prompt_id: handle.prompt_id,
                    generation,
                })
            }
            Err(err) => {
                let err_text = err.to_string();
                if Self::is_busy_gateway_delivery_error(&err_text) {
                    self.mark_gateway_delivery_busy(
                        pane_id,
                        prompt_id.clone(),
                        generation,
                        Instant::now(),
                    );
                    return Ok(PromptDeliveryAttempt::Queued {
                        prompt_id,
                        ahead_of: self.pane_prompt_backlog_with_live_turn(pane_id),
                    });
                }
                let clear_session = !Self::is_nonrecoverable_gateway_delivery_error(&err_text);
                if !clear_session {
                    super::stability::write_agent_health_marker(pane_id, &err_text);
                }
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    if clear_session {
                        pane.clear_gateway_session();
                    }
                    let err_msg =
                        format!("\x1b[1;31mPrompt delivery failed: {}\x1b[0m\r\n", err_text);
                    let _ = pane.append_output(err_msg.as_bytes());
                }
                tracing::warn!(
                    pane = %pane_id,
                    error = %err_text,
                    clear_session,
                    "Agent gateway prompt delivery failed"
                );
                Err(Error::pty(format!(
                    "Gateway prompt delivery failed for {}: {}",
                    pane_id, err_text
                )))
            }
        }
    }

    pub fn finalize_async_gateway_prompt_delivery(
        &mut self,
        pane_id: &str,
        prompt: &str,
        from: Option<&str>,
        result: std::result::Result<(), AsyncGatewayPromptDeliveryError>,
    ) {
        match result {
            Ok(()) => {
                super::stability::clear_agent_health_marker(pane_id);
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    let notice = prompt_delivery_notice(prompt, from);
                    let _ = pane.append_output(notice.as_bytes());
                }
                tracing::info!(pane = %pane_id, "Prompt delivered via async agent gateway");
            }
            Err(err) => {
                let err_text = err.error;
                let clear_session = !Self::is_nonrecoverable_gateway_delivery_error(&err_text);
                if !clear_session {
                    super::stability::write_agent_health_marker(pane_id, &err_text);
                }
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    if clear_session {
                        pane.clear_gateway_session();
                    }
                    let err_msg =
                        format!("\x1b[1;31mPrompt delivery failed: {}\x1b[0m\r\n", err_text);
                    let _ = pane.append_output(err_msg.as_bytes());
                }
                tracing::warn!(
                    pane = %pane_id,
                    error = %err_text,
                    clear_session,
                    "Async agent gateway prompt delivery failed"
                );
            }
        }
    }

    pub fn finalize_async_teams_prompt_delivery(
        &mut self,
        pane_id: &str,
        result: std::result::Result<(), AsyncGatewayPromptDeliveryError>,
    ) {
        if let Err(err) = result
            && let Some(pane) = self.panes.get_mut(pane_id)
        {
            pane.set_pending_inbox_nudge(false);
            let err_msg = format!("\x1b[1;31mPrompt delivery failed: {}\x1b[0m\r\n", err.error);
            let _ = pane.append_output(err_msg.as_bytes());
        }
    }

    pub(super) fn log_teams_inbox_delivery_failure(team: &str, pane_id: &str, error: &str) {
        tracing::error!(
            team = %team,
            agent = %pane_id,
            error = %error,
            "Teams inbox write failed"
        );
    }

    async fn apply_gateway_spawn_config(
        &mut self,
        pane_id: &str,
        session_id: &brehon_types::SessionId,
        spawn_config: &GatewaySpawnConfig,
    ) {
        if !is_opencode_model_config_spawn(spawn_config) {
            return;
        }

        let candidates = opencode_model_candidates(spawn_config);
        if candidates.is_empty() {
            return;
        }

        let Some(gateway) = self.gateway.as_ref() else {
            return;
        };

        let mut last_error: Option<String> = None;
        for candidate in candidates {
            match brehon_ports::AgentGateway::set_config(gateway, session_id, "model", &candidate)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        pane = %pane_id,
                        session = %session_id,
                        model = %candidate,
                        "Applied OpenCode model override"
                    );
                    return;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                    tracing::warn!(
                        pane = %pane_id,
                        session = %session_id,
                        model = %candidate,
                        error = %err,
                        "Failed to apply OpenCode ACP model override candidate"
                    );
                }
            }
        }

        if let Some(err) = last_error {
            let message = format!(
                "Brehon could not apply the OpenCode model override; using default model ({err})."
            );
            if let Some(pane) = self.panes.get_mut(pane_id) {
                let _ = pane.append_output(format!("\x1b[1;31m{message}\x1b[0m\r\n").as_bytes());
            }
        }
    }

    /// Ensure a gateway session exists for the given pane, spawning if needed.
    pub(super) async fn ensure_gateway_session(&mut self, pane_id: &str) -> Result<()> {
        if let Some(reason) = self.pane_death_reason(pane_id) {
            return Err(Self::rejection_error(pane_id, &reason));
        }
        if super::stability::agent_is_marked_unavailable(pane_id) {
            return Err(Error::pty(format!(
                "Agent {pane_id} is quarantined unavailable for this run"
            )));
        }

        // Ensure gateway exists
        if self.gateway.is_none() {
            self.gateway = Some(brehon_acp::AcpGateway::new());
        }

        let existing_session_id = self
            .panes
            .get(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?
            .gateway_session_id()
            .map(brehon_types::SessionId::new);

        if let Some(session_id) = existing_session_id {
            let gateway = self
                .gateway
                .as_ref()
                .ok_or_else(|| Error::pty("Agent gateway not configured"))?;
            let health = brehon_ports::AgentGateway::health_check(gateway, &session_id).await;
            match health {
                Ok(brehon_types::HealthStatus::Healthy | brehon_types::HealthStatus::Unknown) => {
                    return Ok(());
                }
                Ok(brehon_types::HealthStatus::Unhealthy) => {
                    tracing::warn!(
                        pane = %pane_id,
                        session = %session_id,
                        "Resetting unhealthy agent gateway session before prompt delivery"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        pane = %pane_id,
                        session = %session_id,
                        error = %err,
                        "Resetting agent gateway session after failed health check"
                    );
                }
            }

            if let Some(gateway) = self.gateway.as_ref()
                && let Err(err) =
                    brehon_ports::AgentGateway::kill_session(gateway, &session_id).await
            {
                let err_text = err.to_string();
                if !Self::is_missing_gateway_session_error(&err_text) {
                    tracing::warn!(
                        pane = %pane_id,
                        session = %session_id,
                        error = %err,
                        "Failed to kill unhealthy agent gateway session"
                    );
                }
            }

            self.clear_active_gateway_operations(pane_id);
            if let Some(pane) = self.panes.get_mut(pane_id) {
                pane.clear_gateway_session();
                let _ = pane.append_output(
                    b"\x1b[2mBrehon reset a degraded gateway session before continuing.\x1b[0m\r\n",
                );
            }
        }

        let (spawn_config, pane_kind_str) = {
            let pane = self
                .panes
                .get_mut(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;

            // Already has a healthy session — nothing to do
            if pane.gateway_session_id().is_some() {
                return Ok(());
            }

            let spawn_config = pane.gateway_spawn_config().cloned().ok_or_else(|| {
                Error::pty(format!(
                    "Gateway pane '{}' has no spawn config and no session",
                    pane_id
                ))
            })?;
            let pane_kind_str = pane.kind().as_str().to_string();
            (spawn_config, pane_kind_str)
        };

        if self.worktree_isolation
            && matches!(pane_kind_str.as_str(), "worker" | "reviewer" | "supervisor")
        {
            let shared_root = self.shared_repo_root.as_ref().ok_or_else(|| {
                Error::terminal(
                    "Worktree isolation is enabled, but the mux lost track of the shared repo root."
                        .to_string(),
                )
            })?;
            ensure_isolated_cwd_is_not_shared_root(
                shared_root,
                Path::new(&spawn_config.cwd),
                &pane_kind_str,
                pane_id,
            )?;
        }

        let gw = self
            .gateway
            .as_mut()
            .ok_or_else(|| Error::pty("Agent gateway not configured"))?;
        let tool_bridge = if matches!(
            spawn_config.protocol,
            brehon_acp::GatewayProtocol::OpenAiCompatibleChat
        ) {
            self.direct_tool_bridge_factory.as_ref().map(|factory| {
                factory.build(
                    &spawn_config.cwd,
                    &spawn_config.env,
                    spawn_config.tool_prefix.as_deref(),
                )
            })
        } else {
            None
        };
        gw.register_agent_launch(
            pane_id,
            brehon_acp::AgentLaunchConfig {
                command: spawn_config.command.clone(),
                args: spawn_config.args.clone(),
                env: spawn_config.env.clone(),
                protocol: spawn_config.protocol,
                tool_prefix: spawn_config.tool_prefix.clone(),
                tool_bridge,
                base_url: spawn_config.base_url.clone(),
                api_key_env: spawn_config.api_key_env.clone(),
                headers: spawn_config.headers.clone(),
                model: spawn_config.model.clone(),
                sidecar_socket_path: spawn_config.sidecar_socket_path.clone(),
                sidecar_ready_path: spawn_config.sidecar_ready_path.clone(),
                sidecar_connect_timeout_ms: spawn_config.sidecar_connect_timeout_ms,
            },
        );
        let (session_event_tx, session_event_rx) = mpsc::channel(128);
        gw.register_agent_event_channel(pane_id, session_event_tx);

        // Now spawn the session with the launch config registered above.
        let spec = brehon_types::SessionSpec::new(
            brehon_types::AgentId::new(pane_id),
            pane_kind_str.to_string(),
            spawn_config.cwd.clone(),
        );

        let gateway = self
            .gateway
            .as_ref()
            .ok_or_else(|| Error::pty("ACP gateway not configured"))?;
        match brehon_ports::AgentGateway::spawn(gateway, spec).await {
            Ok(session_id) => {
                tracing::info!(
                    pane = %pane_id,
                    session = %session_id,
                    "Agent gateway session spawned"
                );
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.register_gateway_session_spawn(session_id.as_str().to_string());
                    pane.set_gateway_event_bridge_started(true);
                }
                self.publish_runtime_pane_spawned(pane_id);
                self.spawn_acp_event_bridge(pane_id, session_event_rx);
                self.apply_gateway_spawn_config(pane_id, &session_id, &spawn_config)
                    .await;
                Ok(())
            }
            Err(e) => {
                drop(session_event_rx);
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    let err_msg = format!("\x1b[1;31mGateway spawn failed: {}\x1b[0m\r\n", e,);
                    let _ = pane.append_output(err_msg.as_bytes());
                }
                Err(Error::pty(format!(
                    "Failed to spawn gateway session for {}: {}",
                    pane_id, e
                )))
            }
        }
    }

    async fn ensure_gateway_terminal(&mut self, pane_id: &str) -> Result<brehon_types::TerminalId> {
        self.ensure_gateway_session(pane_id).await?;

        if let Some(terminal_id) = self
            .panes
            .get(pane_id)
            .and_then(|pane| pane.gateway_terminal_id())
        {
            return Ok(brehon_types::TerminalId::new(terminal_id));
        }

        let session_id = self
            .panes
            .get(pane_id)
            .and_then(|pane| pane.gateway_session_id())
            .map(brehon_types::SessionId::new)
            .ok_or_else(|| {
                Error::pty(format!("Gateway pane '{}' has no active session", pane_id))
            })?;

        let gateway = self
            .gateway
            .as_ref()
            .ok_or_else(|| Error::pty("ACP gateway not configured"))?;

        match brehon_ports::AgentGateway::attach_terminal(gateway, &session_id).await {
            Ok(Some(terminal_id)) => {
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.set_gateway_terminal_id(terminal_id.as_str().to_string());
                    let msg = format!(
                        "\x1b[1;36m[gateway]\x1b[0m Terminal {} attached\r\n",
                        terminal_id
                    );
                    let _ = pane.append_output(msg.as_bytes());
                }
                Ok(terminal_id)
            }
            Ok(None) => {
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    let msg = "\x1b[1;31m[gateway]\x1b[0m Interactive terminal input is not supported by this agent\r\n";
                    let _ = pane.append_output(msg.as_bytes());
                }
                Err(Error::pty(format!(
                    "ACP agent '{}' does not support interactive terminal input",
                    pane_id
                )))
            }
            Err(err) => {
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    let msg = format!(
                        "\x1b[1;31m[gateway]\x1b[0m Terminal attach failed: {}\r\n",
                        err
                    );
                    let _ = pane.append_output(msg.as_bytes());
                }
                Err(Error::pty(format!(
                    "Failed to attach ACP terminal for {}: {}",
                    pane_id, err
                )))
            }
        }
    }

    async fn send_input_via_gateway(&mut self, pane_id: &str, data: &[u8]) -> Result<()> {
        let terminal_id = self.ensure_gateway_terminal(pane_id).await?;
        let gateway = self
            .gateway
            .as_ref()
            .ok_or_else(|| Error::pty("ACP gateway not configured"))?;

        brehon_ports::AgentGateway::send_terminal_input(gateway, &terminal_id, data.to_vec())
            .await
            .map_err(|err| {
                Error::pty(format!(
                    "ACP terminal input failed for {}: {}",
                    pane_id, err
                ))
            })
    }

    /// Queue a startup prompt for delayed delivery.
    ///
    /// Even Teams-backed Claude panes stay on the delayed queue here. Freshly
    /// restarted Claude sessions can poll their inbox before MCP tools are
    /// ready, so writing startup prompts immediately races the Brehon handshake.
    ///
    /// The delayed queue is drained by the pane state-machine tick path in
    /// `mux/events.rs`, which handles gateway vs Teams vs PTY delivery.
    pub fn queue_startup_prompt(&mut self, pane_id: &str, prompt: String) {
        let pane = self.panes.get(pane_id);
        let control_plane = pane
            .map(|p| p.cli_type().control_plane())
            .unwrap_or(HarnessControlPlane::PtyInjection);

        // All startup prompts use the same delayed queue mechanism.
        // Gateway agents need time for process startup + ACP handshake.
        // Teams-backed Claude panes need time for MCP tools to surface.
        let startup_slot = self.pending_delayed_prompt_count() as u64;
        let delay = std::time::Duration::from_secs(PTY_STARTUP_PROMPT_DELAY_SECS)
            + std::time::Duration::from_millis(
                startup_slot.saturating_mul(STARTUP_PROMPT_STAGGER_MILLIS),
            );
        let transport = match control_plane {
            HarnessControlPlane::Acp
            | HarnessControlPlane::AcpSidecar
            | HarnessControlPlane::OpenAiCompatible => "ACP gateway",
            HarnessControlPlane::NativeHooks if pane.is_some_and(|p| self.pane_uses_teams(p)) => {
                "Teams inbox"
            }
            _ => "PTY injection",
        };
        let generation = self.pane_generation_for_observability(pane_id);
        match self.queue_delayed_prompt(pane_id, prompt, None, Instant::now() + delay, None) {
            PromptDeliveryAttempt::Queued {
                prompt_id,
                ahead_of,
            } => {
                tracing::info!(
                    pane = %pane_id,
                    prompt_id = %prompt_id,
                    generation = generation.0,
                    ahead_of,
                    delay_ms = %delay.as_millis(),
                    "Queued startup prompt for {}",
                    transport
                );
            }
            PromptDeliveryAttempt::AlreadyPresent {
                prompt_id,
                position,
            } => {
                tracing::debug!(
                    pane = %pane_id,
                    prompt_id = %prompt_id,
                    position = %position,
                    "Startup prompt already present for {}",
                    transport
                );
            }
            PromptDeliveryAttempt::Rejected { reason } => {
                tracing::warn!(
                    pane = %pane_id,
                    reason = ?reason,
                    "Rejected startup prompt for {}",
                    transport
                );
            }
            PromptDeliveryAttempt::Delivered { .. } => {}
        }
    }

    /// Advance queued prompt delivery using the pane state-machine tick.
    ///
    /// This is called from the TUI loop after each poll cycle. Busy → Ready
    /// transitions are handled inside `mux/events.rs`, which is the sole
    /// dispatcher for prompts queued in `PanePromptQueue`.
    pub fn flush_pending_startup_prompts(&mut self, rt: &tokio::runtime::Handle) {
        self.tick_pane_state_machine(rt);
    }

    /// Clean up Teams state on shutdown.
    pub fn cleanup_teams(&self) {
        if let Some(ref teams) = self.teams {
            teams.cleanup();
        }
    }

    /// Trigger any Claude Teams inbox nudges that are now safe to send.
    ///
    /// Dispatches via non-blocking tasks so the TUI render loop never stalls.
    pub fn flush_pending_inbox_nudges(&mut self, rt: &tokio::runtime::Handle) {
        let now = Instant::now();
        let ready: Vec<String> = self
            .panes
            .iter()
            .filter(|(pane_id, pane)| {
                self.pane_uses_teams(pane)
                    && pane.pending_inbox_nudge()
                    && self.pane_ready_for_inbox_nudge(pane_id, pane, now)
            })
            .map(|(pane_id, _)| pane_id.clone())
            .collect();
        let forced_supervisor_recovery: Vec<String> = self
            .panes
            .iter()
            .filter(|(pane_id, pane)| {
                self.pane_uses_teams(pane)
                    && pane.pending_inbox_nudge()
                    && pane.kind() == &crate::pane::PaneKind::Supervisor
                    && pane.pending_inbox_nudge_since().is_some_and(|since| {
                        now.saturating_duration_since(since) >= SUPERVISOR_INBOX_ESCALATION_DELAY
                    })
                    && now.saturating_duration_since(pane.last_output_at())
                        >= SUPERVISOR_INBOX_ESCALATION_QUIET_THRESHOLD
                    && !self.pane_ready_for_inbox_nudge(pane_id, pane, now)
            })
            .map(|(pane_id, _)| pane_id.clone())
            .collect();

        for pane_id in ready {
            self.dispatch_nudge_inbox(rt, &pane_id);
            tracing::info!(pane = %pane_id, "Dispatched Teams inbox nudge (non-blocking)");
        }

        for pane_id in forced_supervisor_recovery {
            rt.block_on(self.force_supervisor_inbox_recovery(&pane_id));
        }
    }

    /// State-machine driven recovery for a supervisor pane whose Teams inbox
    /// nudge has been stuck behind a non-empty prompt for longer than
    /// [`SUPERVISOR_INBOX_ESCALATION_DELAY`].
    ///
    /// The previous implementation wrote `Ctrl-U + text + Enter` directly to
    /// the PTY and treated "bytes written" as "prompt delivered". That assumption
    /// fails for Claude Code, whose multi-line Ink input box treats `\r` as a
    /// literal newline rather than submit when the buffer is non-empty, and
    /// where Ctrl-U only kills the current row instead of the whole buffer.
    /// The result was multiple identical recovery messages stacking up
    /// unsent in the supervisor's input box on every retry tick. Recovery is
    /// now control-only: clear a draft with Ctrl-C, then send plain Enter once
    /// an empty prompt is observed so Claude picks up its Teams inbox.
    ///
    /// This implementation inspects the Claude prompt state and dispatches:
    ///
    /// * `Visible` — agent is mid-turn; defer.
    /// * `Draft` — non-empty input; send Ctrl-C (Claude Code's "discard
    ///   draft" affordance) and let the next tick observe `Empty`.
    /// * `Empty` / `None` — send only Enter to nudge Claude's inbox. Never
    ///   type synthetic recovery text into the agent prompt.
    ///
    /// In all branches, [`Pane::set_inbox_nudge_not_before`] is armed with a
    /// short cooldown so the recovery cannot re-fire on every tick while
    /// Claude is still redrawing after our previous keystrokes.
    pub(super) async fn force_supervisor_inbox_recovery(&mut self, pane_id: &str) {
        let now = Instant::now();
        let cooldown_until = now + SUPERVISOR_INBOX_RECOVERY_COOLDOWN;
        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.set_inbox_nudge_not_before(Some(cooldown_until));
        }

        let prompt_state = self.claude_prompt_state_for_pane(pane_id);

        match prompt_state {
            ClaudePromptState::Visible => {
                tracing::debug!(
                    pane = %pane_id,
                    "Deferring supervisor inbox recovery: agent appears to be mid-turn"
                );
            }
            ClaudePromptState::Draft => {
                if self.is_panesmith_managed(pane_id) {
                    match self.send_panesmith_input_transaction(
                        pane_id,
                        panesmith::InputTransaction::interrupt(),
                    ) {
                        Ok(Some(outcome)) => {
                            if outcome.is_success() {
                                tracing::warn!(
                                    pane = %pane_id,
                                    "Cleared stale supervisor draft via Panesmith Ctrl-C transaction; re-entering recovery on next tick"
                                );
                            } else {
                                let error = super::panesmith::ensure_panesmith_mux_outcome(
                                    "supervisor recovery interrupt",
                                    &outcome,
                                )
                                .err();
                                tracing::warn!(
                                    pane = %pane_id,
                                    error = ?error,
                                    outcome = ?outcome,
                                    "Failed to send Panesmith Ctrl-C transaction during supervisor inbox recovery"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(
                                pane = %pane_id,
                                error = %err,
                                "Failed to send Panesmith Ctrl-C transaction during supervisor inbox recovery"
                            );
                        }
                    }
                    return;
                }

                let pane = match self.panes.get(pane_id) {
                    Some(pane) => pane,
                    None => return,
                };
                let injector = match pane.injector_handle() {
                    Some(handle) => handle,
                    None => return,
                };
                if let Err(err) = injector.write(b"\x03").await {
                    tracing::warn!(
                        pane = %pane_id,
                        error = %err,
                        "Failed to send Ctrl-C clear during supervisor inbox recovery"
                    );
                    return;
                }
                tracing::warn!(
                    pane = %pane_id,
                    "Cleared stale supervisor draft via Ctrl-C; re-entering recovery on next tick"
                );
            }
            ClaudePromptState::Empty | ClaudePromptState::None => {
                if self.is_panesmith_managed(pane_id) {
                    match self.send_panesmith_input_transaction(
                        pane_id,
                        super::panesmith::panesmith_enter_transaction(),
                    ) {
                        Ok(Some(outcome)) => {
                            if outcome.is_success() {
                                if let Some(pane) = self.panes.get_mut(pane_id) {
                                    pane.set_pending_inbox_nudge(false);
                                }
                                tracing::warn!(
                                    pane = %pane_id,
                                    "Forced supervisor inbox nudge through Panesmith Enter transaction after Teams inbox remained blocked"
                                );
                            } else {
                                let error = super::panesmith::ensure_panesmith_mux_outcome(
                                    "supervisor recovery inbox nudge",
                                    &outcome,
                                )
                                .err();
                                tracing::warn!(
                                    pane = %pane_id,
                                    error = ?error,
                                    outcome = ?outcome,
                                    "Failed to force supervisor inbox nudge through Panesmith Enter transaction"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(
                                pane = %pane_id,
                                error = %err,
                                "Failed to force supervisor inbox nudge through Panesmith Enter transaction"
                            );
                        }
                    }
                    return;
                }

                let pane = match self.panes.get(pane_id) {
                    Some(pane) => pane,
                    None => return,
                };
                let injector = match pane.injector_handle() {
                    Some(handle) => handle,
                    None => return,
                };
                match injector.nudge_inbox().await {
                    Ok(()) => {
                        if let Some(pane) = self.panes.get_mut(pane_id) {
                            pane.set_pending_inbox_nudge(false);
                        }
                        tracing::warn!(
                            pane = %pane_id,
                            "Forced supervisor inbox nudge after Teams inbox remained blocked"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            pane = %pane_id,
                            error = %err,
                            "Failed to force supervisor inbox nudge for stuck Teams inbox"
                        );
                    }
                }
            }
        }
    }
}
