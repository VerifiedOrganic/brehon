//! Reset, crash detection, health markers, and pane-busy probes.

use crate::error::{Error, Result};
use crate::pane::{PaneKind, PaneState};
use brehon_types::RuntimeCommandKind;
use std::path::PathBuf;

use super::Mux;

// ── Health marker helpers ───────────────────────────────────────────────────

fn agent_health_dir() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .filter(|root| !root.trim().is_empty())
        .map(PathBuf::from)
        .map(|root| root.join("runtime").join("agent-health"))
}

fn agent_health_path(agent_name: &str) -> Option<PathBuf> {
    let mut file_name = String::new();
    for ch in agent_name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            file_name.push(ch);
        } else {
            file_name.push('_');
        }
    }
    agent_health_dir().map(|dir| dir.join(format!("{file_name}.json")))
}

pub(crate) fn write_agent_health_marker(agent_name: &str, error: &str) {
    let Some(dir) = agent_health_dir() else {
        return;
    };
    let Some(path) = agent_health_path(agent_name) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let payload = serde_json::json!({
        "agent": agent_name,
        "status": "unavailable",
        "reason": "nonrecoverable_delivery_failure",
        "error": error,
        "updated_at": chrono::Utc::now().to_rfc3339(),
    });
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string()),
    );
}

pub(crate) fn clear_agent_health_marker(agent_name: &str) {
    let Some(path) = agent_health_path(agent_name) else {
        return;
    };
    let _ = std::fs::remove_file(path);
}

pub(crate) fn agent_is_marked_unavailable(agent_name: &str) -> bool {
    let Some(path) = agent_health_path(agent_name) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    value.get("status").and_then(|status| status.as_str()) == Some("unavailable")
}

// ── impl Mux stability methods ──────────────────────────────────────────────

impl Mux {
    fn restart_local_terminal_after_reset(&mut self, pane_id: &str, role: &str) -> Result<bool> {
        let is_panesmith_managed = self
            .panes
            .get(pane_id)
            .is_some_and(|pane| pane.is_panesmith_managed());

        if is_panesmith_managed {
            self.kill_panesmith_pane(pane_id)?;
            match self.restart_panesmith_for_existing_pane(pane_id) {
                Ok(()) => return Ok(true),
                Err(err) => {
                    tracing::warn!(
                        pane = %pane_id,
                        role,
                        error = %err,
                        "Panesmith reset restart failed; falling back to ghostty_vt PTY path"
                    );
                }
            }
        }

        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.set_panesmith_managed(false);
        pane.restart_pty_from_spawn_config()?;
        Ok(false)
    }

    fn mark_pane_ready_after_reset(&mut self, pane_id: &str, reason: &str) {
        let state_change = {
            let Some(pane) = self.panes.get_mut(pane_id) else {
                return;
            };
            let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
            let generation = pane.current_generation();
            pane.exited = false;
            pane.exit_code = None;
            pane.set_pane_state(PaneState::Ready {
                since: std::time::Instant::now(),
            });
            Self::runtime_state_change(previous, pane.pane_state(), reason)
                .map(|(previous, current, reason)| (generation, previous, current, reason))
        };

        if let Some((generation, previous, current, reason)) = state_change {
            self.publish_runtime_pane_state_changed(
                pane_id,
                generation,
                previous,
                current,
                Some(reason),
            );
        }
    }

    /// Hard-reset a reviewer session while keeping the visible pane slot.
    ///
    /// Gateway reviewers have their session killed and restarted on demand.
    /// Native PTY reviewers (for example Claude Code with Teams inbox delivery)
    /// are restarted from their stored PTY spawn config to clear conversation
    /// state between shared review assignments.
    pub async fn reset_reviewer_session(&mut self, pane_id: &str) -> Result<()> {
        self.ensure_pane_uses_isolated_cwd(pane_id, "reviewer")?;
        let (is_gateway_backed, gateway_session_id) = {
            let pane = self
                .panes
                .get(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;
            if pane.kind() != &PaneKind::Reviewer {
                return Err(Error::pty(format!(
                    "Pane '{pane_id}' is not a reviewer and cannot be reset as reviewer state"
                )));
            }
            (
                pane.is_gateway_backed(),
                pane.gateway_session_id().map(brehon_types::SessionId::new),
            )
        };
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::ResetPane {
                reason: "reset reviewer session".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("reviewer reset", &decision) {
            return Err(err);
        }

        if is_gateway_backed
            && let Some(session_id) = gateway_session_id
            && let Some(gateway) = self.gateway.as_ref()
            && let Err(err) = brehon_ports::AgentGateway::kill_session(gateway, &session_id).await
        {
            let err_text = err.to_string();
            let lower = err_text.to_ascii_lowercase();
            if !(lower.contains("not found") || lower.contains("unknown session")) {
                return Err(Error::pty(format!(
                    "Failed to kill reviewer gateway session for {pane_id}: {err_text}"
                )));
            }
        }

        self.clear_active_gateway_operations(pane_id);
        let restarted_with_panesmith = if is_gateway_backed {
            false
        } else {
            self.restart_local_terminal_after_reset(pane_id, "reviewer")?
        };
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if is_gateway_backed {
                pane.clear_gateway_session();
                if let Some(activity) = pane.activity_buffer_mut() {
                    activity.clear();
                }
            }
            pane.clear_review_context();
            pane.set_tool_executing(false);
            let notice = if is_gateway_backed {
                "\x1b[2mBrehon reset reviewer session after completed submission. Starting a fresh review context.\x1b[0m\r\n"
            } else if restarted_with_panesmith {
                ""
            } else {
                "\x1b[2mBrehon restarted reviewer process after completed submission. Starting a fresh review context.\x1b[0m\r\n"
            };
            if !notice.is_empty() {
                let _ = pane.append_output(notice.as_bytes());
            }
        }
        self.mark_pane_ready_after_reset(pane_id, "reviewer session reset");
        clear_agent_health_marker(pane_id);
        let _ = self
            .event_tx
            .try_send(super::MuxEvent::ReviewContextChanged {
                pane_id: pane_id.to_string(),
                context: None,
            });

        Ok(())
    }

    /// Hard-reset an advisor session while keeping the visible pane slot.
    ///
    /// Advisors are read-only room participants; resetting clears the model
    /// conversation without touching shared task or review state.
    pub async fn reset_advisor_session(&mut self, pane_id: &str) -> Result<()> {
        self.ensure_pane_uses_isolated_cwd(pane_id, "advisor")?;
        let (is_gateway_backed, gateway_session_id) = {
            let pane = self
                .panes
                .get(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;
            if pane.kind() != &PaneKind::Advisor {
                return Err(Error::pty(format!(
                    "Pane '{pane_id}' is not an advisor and cannot be reset as advisor state"
                )));
            }
            (
                pane.is_gateway_backed(),
                pane.gateway_session_id().map(brehon_types::SessionId::new),
            )
        };
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::ResetPane {
                reason: "reset advisor session".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("advisor reset", &decision) {
            return Err(err);
        }

        if is_gateway_backed
            && let Some(session_id) = gateway_session_id
            && let Some(gateway) = self.gateway.as_ref()
            && let Err(err) = brehon_ports::AgentGateway::kill_session(gateway, &session_id).await
        {
            let err_text = err.to_string();
            let lower = err_text.to_ascii_lowercase();
            if !(lower.contains("not found") || lower.contains("unknown session")) {
                return Err(Error::pty(format!(
                    "Failed to kill advisor gateway session for {pane_id}: {err_text}"
                )));
            }
        }

        self.clear_active_gateway_operations(pane_id);
        let restarted_with_panesmith = if is_gateway_backed {
            false
        } else {
            self.restart_local_terminal_after_reset(pane_id, "advisor")?
        };
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if is_gateway_backed {
                pane.clear_gateway_session();
                if let Some(activity) = pane.activity_buffer_mut() {
                    activity.clear();
                }
            }
            pane.set_tool_executing(false);
            pane.set_pending_inbox_nudge(false);
            let notice = if is_gateway_backed {
                "\x1b[2mBrehon reset advisor session. Rejoining advisor rooms with a fresh context.\x1b[0m\r\n"
            } else if restarted_with_panesmith {
                ""
            } else {
                "\x1b[2mBrehon restarted advisor process. Rejoining advisor rooms with a fresh context.\x1b[0m\r\n"
            };
            if !notice.is_empty() {
                let _ = pane.append_output(notice.as_bytes());
            }
        }
        self.mark_pane_ready_after_reset(pane_id, "advisor session reset");
        clear_agent_health_marker(pane_id);

        Ok(())
    }

    /// Hard-reset a research session while keeping the visible pane slot.
    ///
    /// Research agents are read-only artifact producers; resetting clears the
    /// model conversation without touching queued jobs or submitted artifacts.
    pub async fn reset_research_session(&mut self, pane_id: &str) -> Result<()> {
        self.ensure_pane_uses_isolated_cwd(pane_id, "research")?;
        let (is_gateway_backed, gateway_session_id) = {
            let pane = self
                .panes
                .get(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;
            if pane.kind() != &PaneKind::Research {
                return Err(Error::pty(format!(
                    "Pane '{pane_id}' is not a research agent and cannot be reset as research state"
                )));
            }
            (
                pane.is_gateway_backed(),
                pane.gateway_session_id().map(brehon_types::SessionId::new),
            )
        };
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::ResetPane {
                reason: "reset research session".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("research reset", &decision) {
            return Err(err);
        }

        if is_gateway_backed
            && let Some(session_id) = gateway_session_id
            && let Some(gateway) = self.gateway.as_ref()
            && let Err(err) = brehon_ports::AgentGateway::kill_session(gateway, &session_id).await
        {
            let err_text = err.to_string();
            let lower = err_text.to_ascii_lowercase();
            if !(lower.contains("not found") || lower.contains("unknown session")) {
                return Err(Error::pty(format!(
                    "Failed to kill research gateway session for {pane_id}: {err_text}"
                )));
            }
        }

        self.clear_active_gateway_operations(pane_id);
        let restarted_with_panesmith = if is_gateway_backed {
            false
        } else {
            self.restart_local_terminal_after_reset(pane_id, "research")?
        };
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if is_gateway_backed {
                pane.clear_gateway_session();
                if let Some(activity) = pane.activity_buffer_mut() {
                    activity.clear();
                }
            }
            pane.set_tool_executing(false);
            pane.set_pending_inbox_nudge(false);
            let notice = if is_gateway_backed {
                "\x1b[2mBrehon reset research session. Rejoining the research queue with a fresh context.\x1b[0m\r\n"
            } else if restarted_with_panesmith {
                ""
            } else {
                "\x1b[2mBrehon restarted research process. Rejoining the research queue with a fresh context.\x1b[0m\r\n"
            };
            if !notice.is_empty() {
                let _ = pane.append_output(notice.as_bytes());
            }
        }
        self.mark_pane_ready_after_reset(pane_id, "research session reset");
        clear_agent_health_marker(pane_id);

        Ok(())
    }

    /// Hard-reset a supervisor session while keeping the visible pane slot.
    ///
    /// Gateway-backed supervisors have their session killed and restarted on
    /// demand. Native PTY supervisors (for example Claude Code with Teams
    /// inbox delivery) are restarted from their stored PTY spawn config to
    /// recover from wedged runtime/UI states.
    pub async fn reset_supervisor_session(&mut self, pane_id: &str) -> Result<()> {
        self.ensure_pane_uses_isolated_cwd(pane_id, "supervisor")?;
        let (is_gateway_backed, gateway_session_id) = {
            let pane = self
                .panes
                .get(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;
            if pane.kind() != &PaneKind::Supervisor {
                return Err(Error::pty(format!(
                    "Pane '{pane_id}' is not a supervisor and cannot be reset as supervisor state"
                )));
            }
            (
                pane.is_gateway_backed(),
                pane.gateway_session_id().map(brehon_types::SessionId::new),
            )
        };
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::ResetPane {
                reason: "reset supervisor session".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("supervisor reset", &decision) {
            return Err(err);
        }

        if is_gateway_backed
            && let Some(session_id) = gateway_session_id
            && let Some(gateway) = self.gateway.as_ref()
            && let Err(err) = brehon_ports::AgentGateway::kill_session(gateway, &session_id).await
        {
            let err_text = err.to_string();
            let lower = err_text.to_ascii_lowercase();
            if !(lower.contains("not found") || lower.contains("unknown session")) {
                return Err(Error::pty(format!(
                    "Failed to kill supervisor gateway session for {pane_id}: {err_text}"
                )));
            }
        }

        self.clear_active_gateway_operations(pane_id);
        let restarted_with_panesmith = if is_gateway_backed {
            false
        } else {
            self.restart_local_terminal_after_reset(pane_id, "supervisor")?
        };
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if is_gateway_backed {
                pane.clear_gateway_session();
                if let Some(activity) = pane.activity_buffer_mut() {
                    activity.clear();
                }
            }
            pane.set_tool_executing(false);
            pane.set_pending_inbox_nudge(false);
            let notice = if is_gateway_backed {
                "\x1b[2mBrehon reset supervisor session after a runtime failure. Restarting with a fresh supervisor context.\x1b[0m\r\n"
            } else if restarted_with_panesmith {
                ""
            } else {
                "\x1b[2mBrehon restarted supervisor process after a runtime failure. Restarting with a fresh supervisor context.\x1b[0m\r\n"
            };
            if !notice.is_empty() {
                let _ = pane.append_output(notice.as_bytes());
            }
        }
        self.mark_pane_ready_after_reset(pane_id, "supervisor session reset");
        clear_agent_health_marker(pane_id);

        Ok(())
    }

    /// Hard-reset a worker session while keeping the pane slot and task
    /// assignment intact.
    ///
    /// Gateway-backed workers have their session killed and restarted on
    /// demand. Native PTY workers are restarted from their stored PTY spawn
    /// config. Used for recovery from provider failures such as context-length
    /// overruns where a fresh session against the same worktree can continue
    /// safely.
    pub async fn reset_worker_gateway_session(&mut self, pane_id: &str) -> Result<()> {
        self.ensure_pane_uses_isolated_cwd(pane_id, "worker")?;
        let (is_gateway_backed, gateway_session_id) = {
            let pane = self
                .panes
                .get(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;
            if pane.kind() != &PaneKind::Worker {
                return Err(Error::pty(format!(
                    "Pane '{pane_id}' is not a worker and cannot be reset as worker state"
                )));
            }
            (
                pane.is_gateway_backed(),
                pane.gateway_session_id().map(brehon_types::SessionId::new),
            )
        };
        let command = self.runtime_command_for_pane(
            pane_id,
            RuntimeCommandKind::ResetPane {
                reason: "reset worker session".to_string(),
            },
        );
        let context = self.runtime_policy_context_for_pane(pane_id);
        let decision = self.evaluate_runtime_policy(command, context).await;
        if let Some(err) = Self::policy_decision_error("worker reset", &decision) {
            return Err(err);
        }

        if is_gateway_backed
            && let Some(session_id) = gateway_session_id
            && let Some(gateway) = self.gateway.as_ref()
            && let Err(err) = brehon_ports::AgentGateway::kill_session(gateway, &session_id).await
        {
            let err_text = err.to_string();
            let lower = err_text.to_ascii_lowercase();
            if !(lower.contains("not found") || lower.contains("unknown session")) {
                return Err(Error::pty(format!(
                    "Failed to kill worker gateway session for {pane_id}: {err_text}"
                )));
            }
        }

        self.clear_active_gateway_operations(pane_id);
        let restarted_with_panesmith = if is_gateway_backed {
            false
        } else {
            self.restart_local_terminal_after_reset(pane_id, "worker")?
        };
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if is_gateway_backed {
                pane.clear_gateway_session();
                if let Some(activity) = pane.activity_buffer_mut() {
                    activity.clear();
                }
            }
            pane.set_tool_executing(false);
            pane.set_pending_inbox_nudge(false);
            let notice = if is_gateway_backed {
                "\x1b[2mBrehon reset worker session after a model context error. Restarting the worker on the same task/worktree.\x1b[0m\r\n"
            } else if restarted_with_panesmith {
                ""
            } else {
                "\x1b[2mBrehon restarted worker process after a model context error. Restarting the worker on the same task/worktree.\x1b[0m\r\n"
            };
            if !notice.is_empty() {
                let _ = pane.append_output(notice.as_bytes());
            }
        }
        self.mark_pane_ready_after_reset(pane_id, "worker session reset");
        clear_agent_health_marker(pane_id);

        Ok(())
    }

    pub fn pane_has_live_gateway_turn(&self, pane_id: &str) -> bool {
        self.panes
            .get(pane_id)
            .is_some_and(|pane| matches!(pane.pane_state(), Some(PaneState::Busy { .. })))
    }

    pub(crate) fn is_nonrecoverable_gateway_delivery_error(error: &str) -> bool {
        let lower = error.to_ascii_lowercase();
        [
            "pane not found",
            "session not found",
            "unknown agent",
            "exhausted your capacity",
            "quota will reset",
            "rate limit",
            "rate-limit",
            "too many requests",
            "authentication",
            "unauthorized",
            "forbidden",
            "invalid api key",
            "api key",
            "billing",
            "credit",
            "model not found",
            "not enabled",
            "access denied",
            "insufficient permissions",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    }

    pub(crate) fn is_busy_gateway_delivery_error(error: &str) -> bool {
        let lower = error.to_ascii_lowercase();
        lower.contains("prompt already in progress")
            || lower.contains("active prompt")
            || lower.contains("prompt in progress")
    }
}
