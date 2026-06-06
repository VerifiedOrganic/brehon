use super::{MAX_TURN_DURATION, Mux};
use crate::pane::{DeathReason, PaneState};
use std::time::{Duration, Instant};

impl Mux {
    const AGY_HELPER_INFLIGHT_TIMEOUT: Duration = Duration::from_secs(45);

    fn runtime_agent_file_name(agent_name: &str) -> String {
        agent_name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn brehon_runtime_agent_path(kind: &str, agent_name: &str) -> Option<std::path::PathBuf> {
        let root = std::env::var("BREHON_ROOT").ok()?;
        Some(
            std::path::PathBuf::from(root)
                .join("runtime")
                .join(kind)
                .join(Self::runtime_agent_file_name(agent_name)),
        )
    }

    fn read_successful_mcp_timestamp_as_instant(agent_name: &str, now: Instant) -> Option<Instant> {
        let path = Self::brehon_runtime_agent_path("last-successful-mcp", agent_name)?;
        let content = std::fs::read_to_string(path).ok()?;
        let dt = chrono::DateTime::parse_from_rfc3339(content.trim()).ok()?;
        let elapsed = chrono::Utc::now()
            .signed_duration_since(dt.with_timezone(&chrono::Utc))
            .to_std()
            .ok()?;
        Some(now.checked_sub(elapsed).unwrap_or(now))
    }

    fn agy_helper_inflight_age(agent_name: &str) -> Option<Duration> {
        let path = Self::brehon_runtime_agent_path("mcp-helper-inflight", agent_name)?;
        let content = std::fs::read_to_string(path).ok()?;
        let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;
        let started_at = value.get("started_at").and_then(|value| value.as_str())?;
        let dt = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
        chrono::Utc::now()
            .signed_duration_since(dt.with_timezone(&chrono::Utc))
            .to_std()
            .ok()
    }

    fn clear_agy_helper_inflight_marker(agent_name: &str) {
        if let Some(path) = Self::brehon_runtime_agent_path("mcp-helper-inflight", agent_name) {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(super) fn agy_recovery_reason_counts_as_crash(reason: &str) -> bool {
        matches!(reason, "process_exited")
    }

    pub(super) fn tick_agy_or_opencode_supervisor_recovery(
        &mut self,
        rt: &tokio::runtime::Handle,
        pane_id: &str,
        now: Instant,
    ) {
        self.reset_stable_supervisor_crash_counter(pane_id, now);
        self.refresh_supervisor_last_successful_mcp_call(pane_id, now);

        let Some((recover_reason, error_msg)) = self.supervisor_recovery_trigger(pane_id, now)
        else {
            return;
        };

        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.blocked_dead_unavailable_reason = Some(error_msg.clone());
        }
        if recover_reason == "helper_call_hung" {
            Self::clear_agy_helper_inflight_marker(pane_id);
        }

        let consecutive_crashes = self
            .panes
            .get(pane_id)
            .map(|pane| pane.consecutive_crashes)
            .unwrap_or(0);
        let counts_as_crash = Self::agy_recovery_reason_counts_as_crash(&recover_reason);
        if counts_as_crash && consecutive_crashes >= 2 {
            self.quarantine_repeated_supervisor_crash(
                pane_id,
                &recover_reason,
                &error_msg,
                consecutive_crashes,
            );
            return;
        }

        self.recycle_supervisor_after_backoff(
            rt,
            pane_id,
            &recover_reason,
            counts_as_crash,
            consecutive_crashes,
            now,
        );
    }

    fn reset_stable_supervisor_crash_counter(&mut self, pane_id: &str, now: Instant) {
        if let Some(pane) = self.panes.get_mut(pane_id)
            && pane.is_agy_or_opencode_supervisor()
            && !pane.exited
            && !matches!(pane.pane_state(), Some(PaneState::Dead { .. }))
            && let Some(last_restart) = pane.last_restart_at
            && now.saturating_duration_since(last_restart) >= Duration::from_secs(60)
        {
            pane.consecutive_crashes = 0;
        }
    }

    fn refresh_supervisor_last_successful_mcp_call(&mut self, pane_id: &str, now: Instant) {
        if let Some(pane) = self.panes.get_mut(pane_id)
            && pane.is_agy_or_opencode_supervisor()
            && let Some(last_mcp) = Self::read_successful_mcp_timestamp_as_instant(pane_id, now)
        {
            pane.last_successful_mcp_call = Some(last_mcp);
        }
    }

    fn supervisor_recovery_trigger(&self, pane_id: &str, now: Instant) -> Option<(String, String)> {
        let pane = self.panes.get(pane_id)?;
        if !pane.is_agy_or_opencode_supervisor() {
            return None;
        }

        if !matches!(
            pane.pane_state(),
            Some(PaneState::Dead {
                reason: DeathReason::Quarantined(_),
                ..
            })
        ) && (pane.exited || matches!(pane.pane_state(), Some(PaneState::Dead { .. })))
        {
            return Some((
                "process_exited".to_string(),
                "Process exited or entered terminal Dead state".to_string(),
            ));
        }

        if let Some(PaneState::Busy { delivered_at, .. }) = pane.pane_state() {
            let duration_busy = now.saturating_duration_since(*delivered_at);
            if duration_busy >= MAX_TURN_DURATION {
                return Some((
                    "max_turn_exceeded".to_string(),
                    format!("Pane busy beyond maximum turn duration ({MAX_TURN_DURATION:?})"),
                ));
            }
            if duration_busy >= Duration::from_secs(60) && pane.last_output_at <= *delivered_at {
                return Some((
                    "no_output_after_delivery".to_string(),
                    "No output produced within expected startup/turn window".to_string(),
                ));
            }
        }

        if pane.is_agy()
            && let Some(age) = Self::agy_helper_inflight_age(pane_id)
            && age >= Self::AGY_HELPER_INFLIGHT_TIMEOUT
        {
            return Some((
                "helper_call_hung".to_string(),
                format!(
                    "MCP helper remained in-flight for at least {:?}",
                    Self::AGY_HELPER_INFLIGHT_TIMEOUT
                ),
            ));
        }

        if terminal_is_blocked_on_trust_prompt(pane) {
            return Some((
                "blocked_on_approval".to_string(),
                "Terminal prompt is blocked on an approval/trust prompt".to_string(),
            ));
        }

        None
    }

    fn quarantine_repeated_supervisor_crash(
        &mut self,
        pane_id: &str,
        recover_reason: &str,
        error_msg: &str,
        consecutive_crashes: u32,
    ) {
        let death_reason = DeathReason::Quarantined(format!(
            "Repeated crashes (consecutive_crashes={consecutive_crashes})"
        ));
        self.quarantine(pane_id, death_reason);
        if let Some(pane) = self.panes.get(pane_id) {
            super::stability::write_agy_health_marker(pane, recover_reason, error_msg);
        }
    }

    fn recycle_supervisor_after_backoff(
        &mut self,
        rt: &tokio::runtime::Handle,
        pane_id: &str,
        recover_reason: &str,
        counts_as_crash: bool,
        consecutive_crashes: u32,
        now: Instant,
    ) {
        let last_restart = self
            .panes
            .get(pane_id)
            .and_then(|pane| pane.last_restart_at);
        let backoff = if counts_as_crash {
            match consecutive_crashes {
                0 => Duration::from_secs(0),
                1 => Duration::from_secs(2),
                2 => Duration::from_secs(5),
                _ => Duration::from_secs(10),
            }
        } else {
            Duration::from_secs(0)
        };
        let can_recycle = match last_restart {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= backoff,
        };

        if can_recycle {
            tracing::info!(
                pane = %pane_id,
                reason = %recover_reason,
                consecutive_crashes,
                "Triggering autonomous recovery recycle"
            );
            rt.block_on(self.recycle(pane_id, recover_reason));
        } else {
            tracing::debug!(
                    pane = %pane_id,
                    consecutive_crashes,
                    "Postponing recycle due to backoff"
            );
        }
    }

    pub(super) fn drop_stale_in_flight_prompt_after_recycle(pane: &mut crate::pane::Pane) {
        let pane_generation = pane.current_generation();
        let stale = pane
            .prompt_queue
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| in_flight.generation < pane_generation);
        if !stale {
            return;
        }

        let dropped = pane.prompt_queue.in_flight.take().unwrap();
        tracing::info!(
            pane_id = %pane.id,
            prompt_id = %dropped.prompt_id,
            prompt_gen = dropped.generation.0,
            pane_gen = pane_generation.0,
            "dropped stale queued prompt after recycle"
        );
    }
}

fn terminal_is_blocked_on_trust_prompt(pane: &crate::pane::Pane) -> bool {
    let Ok(rows) = pane.viewport_rows_for_display() else {
        return false;
    };

    rows.iter().any(|row| {
        let text = row.text.to_ascii_lowercase();
        (text.contains("trust")
            || text.contains("permission")
            || text.contains("approve")
            || text.contains("allow")
            || text.contains("confirm"))
            && (text.contains("?")
                || text.contains("[y/n]")
                || text.contains("(y/n)")
                || text.contains("yes/no"))
    })
}
