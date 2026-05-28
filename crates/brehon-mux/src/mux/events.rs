//! Event routing, polling, and the ACP event bridge.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::Mux;
use super::format::{
    format_acp_session_event, normalize_gateway_tool_event, session_event_to_activity_entry,
};
use super::types::{GATEWAY_PROMPT_RETRY_DELAY, MuxEvent, PromptDeliveryAttempt};
use crate::pane::{
    ActivityEntry, ActivityKind, DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP, Generation, PaneId,
    PaneState, QueuedPrompt,
};
use crate::pty::PtyEvent;
use brehon_types::{PromptId, RuntimePaneBlockInfo, RuntimePaneBlockKind, RuntimePaneState};

impl Mux {
    const TERMINAL_PROMPT_PREFILTER_TAIL_CHARS: usize = 64;
    const TERMINAL_PROMPT_PREFILTER_TAIL_BYTES: usize =
        Self::TERMINAL_PROMPT_PREFILTER_TAIL_CHARS * 4;
    const TERMINAL_PROMPT_VIEWPORT_SCAN_LINES: usize = 6;

    pub(crate) fn clear_active_gateway_operations(&mut self, pane_id: &str) {
        self.active_gateway_operations.remove(pane_id);
    }

    /// Clear stale structured-activity locks that would otherwise keep a pane
    /// marked as tool-executing forever after a missed completion event.
    ///
    /// Returns `(pane_id, cleared_tool_ids, cleared_operation_lock, still_busy)`
    /// entries for panes where a stale lock was removed.
    pub fn sweep_stale_activity_locks(
        &mut self,
        threshold: Duration,
    ) -> Vec<(PaneId, Vec<String>, bool, bool)> {
        let now = Instant::now();
        let pane_ids = self.panes.keys().cloned().collect::<Vec<_>>();
        let mut cleared = Vec::new();
        let mut state_changes = Vec::new();

        for pane_id in pane_ids {
            let operation_stale = self.active_gateway_operations.contains_key(&pane_id)
                && self.panes.get(&pane_id).is_some_and(|pane| {
                    now.saturating_duration_since(pane.last_output_at()) > threshold
                });
            if operation_stale {
                self.active_gateway_operations.remove(&pane_id);
            }

            let Some(pane) = self.panes.get_mut(&pane_id) else {
                continue;
            };
            let stale_tools = pane
                .activity_buffer_mut()
                .map(|buffer| buffer.sweep_stale(threshold))
                .unwrap_or_default();

            if stale_tools.is_empty() && !operation_stale {
                continue;
            }

            let tools_active = pane
                .activity_buffer()
                .is_some_and(|buffer| buffer.has_in_flight_tools());
            let operations_active = self
                .active_gateway_operations
                .get(&pane_id)
                .copied()
                .unwrap_or(0)
                > 0;
            let still_busy = tools_active || operations_active;
            pane.set_tool_executing(still_busy);
            if !still_busy {
                let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
                let generation = pane.current_generation();
                pane.set_pane_ready(now);
                if let Some((previous, current, reason, blocked)) = Self::runtime_state_change(
                    previous,
                    pane.pane_state(),
                    "stale activity cleared",
                ) {
                    state_changes.push((
                        pane_id.clone(),
                        generation,
                        previous,
                        current,
                        reason,
                        blocked,
                    ));
                }
            }
            cleared.push((pane_id, stale_tools, operation_stale, still_busy));
        }

        for (pane_id, generation, previous, current, reason, blocked) in state_changes {
            self.publish_runtime_pane_state_changed(
                &pane_id,
                generation,
                previous,
                current,
                Some(reason),
                blocked,
            );
        }

        cleared
    }

    fn synthetic_busy_prompt_id(prefix: &str, pane_id: &str) -> PromptId {
        PromptId::new(format!("{prefix}:{pane_id}:{}", uuid::Uuid::new_v4()))
    }

    fn truncate_blocked_text(value: &str, max_chars: usize) -> String {
        let mut out = String::new();
        for (idx, ch) in value.chars().enumerate() {
            if idx >= max_chars {
                out.push('…');
                break;
            }
            out.push(ch);
        }
        out
    }

    fn pane_prompt_id_for_blocked(pane: &crate::pane::Pane) -> Option<String> {
        match pane.pane_state() {
            Some(PaneState::Busy { prompt_id, .. }) => Some(prompt_id.to_string()),
            Some(PaneState::Blocked { info, .. }) => info.request_id.clone(),
            _ => None,
        }
    }

    fn terminal_prompt_keywords() -> &'static [&'static str] {
        &[
            "permission request",
            "requires approval",
            "do you want to allow",
            "approve this command",
            "allow this command",
            "grant access",
            "grant permission",
        ]
    }

    fn terminal_prompt_signal_tokens() -> &'static [&'static str] {
        &[
            "permission",
            "request",
            "requires",
            "approval",
            "approve",
            "allow",
            "grant",
        ]
    }

    fn terminal_provider_context_limit_keywords() -> &'static [&'static str] {
        &[
            "context window exceeds limit",
            "context window exceeds",
            "context window exceeded",
            "context length exceeded",
            "maximum context length",
            "context limit exceeded",
            "token limit exceeded",
        ]
    }

    fn terminal_provider_context_limit_signal_tokens() -> &'static [&'static str] {
        &["context", "token limit", "maximum context"]
    }

    fn ascii_insensitive_contains(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
    }

    fn ascii_insensitive_window_contains(prefix: &str, suffix: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        let prefix = prefix.as_bytes();
        let total_len = prefix.len() + suffix.len();
        if total_len < needle.len() {
            return false;
        }
        (0..=total_len - needle.len()).any(|start| {
            needle.iter().enumerate().all(|(offset, expected)| {
                let idx = start + offset;
                let byte = if idx < prefix.len() {
                    prefix[idx]
                } else {
                    suffix[idx - prefix.len()]
                };
                byte.eq_ignore_ascii_case(expected)
            })
        })
    }

    fn ascii_insensitive_starts_with(text: &str, prefix: &str) -> bool {
        text.as_bytes()
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix.as_bytes()))
    }

    fn ascii_insensitive_suffix_after_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
        if !Self::ascii_insensitive_starts_with(text, prefix) {
            return None;
        }
        text.get(prefix.len()..)
    }

    fn terminal_prompt_prefilter_window(pane: &crate::pane::Pane, data: &[u8]) -> String {
        let mut text =
            String::with_capacity(pane.terminal_prompt_prefilter_tail.len() + data.len());
        text.push_str(&pane.terminal_prompt_prefilter_tail);
        text.push_str(&String::from_utf8_lossy(data));
        text
    }

    fn update_terminal_prompt_prefilter_tail(tail: &mut String, data: &[u8]) {
        tail.push_str(&String::from_utf8_lossy(
            Self::terminal_prompt_prefilter_tail_data(data),
        ));
        if tail.chars().count() > Self::TERMINAL_PROMPT_PREFILTER_TAIL_CHARS {
            *tail = Self::terminal_prompt_prefilter_tail(tail);
        }
    }

    fn terminal_prompt_prefilter_tail_data(data: &[u8]) -> &[u8] {
        if data.len() <= Self::TERMINAL_PROMPT_PREFILTER_TAIL_BYTES {
            return data;
        }
        let mut start = data.len() - Self::TERMINAL_PROMPT_PREFILTER_TAIL_BYTES;
        while start < data.len() && (data[start] & 0b1100_0000) == 0b1000_0000 {
            start += 1;
        }
        &data[start..]
    }

    fn terminal_prompt_prefilter_tail(text: &str) -> String {
        let mut tail = String::new();
        for (idx, ch) in text.chars().rev().enumerate() {
            if idx >= Self::TERMINAL_PROMPT_PREFILTER_TAIL_CHARS {
                break;
            }
            tail.push(ch);
        }
        tail.chars().rev().collect()
    }

    fn terminal_prompt_signal_line<'a>(text: &'a str) -> Option<&'a str> {
        text.lines().rev().map(str::trim).find(|line| {
            !line.is_empty()
                && !Self::terminal_prompt_status_line_is_informational(line)
                && Self::terminal_prompt_keywords().iter().any(|needle| {
                    Self::ascii_insensitive_contains(line.as_bytes(), needle.as_bytes())
                })
        })
    }

    fn terminal_provider_context_limit_line(text: &str) -> Option<&str> {
        text.lines().rev().map(str::trim).find(|line| {
            if line.is_empty() {
                return false;
            }
            let stripped = Self::strip_ansi_escape_sequences(line);
            Self::terminal_provider_context_limit_keywords()
                .iter()
                .any(|needle| {
                    Self::ascii_insensitive_contains(stripped.as_bytes(), needle.as_bytes())
                })
        })
    }

    fn terminal_prompt_informational_status_prefixes() -> &'static [&'static str] {
        &[
            "auto-approved gemini permission request",
            "rejected gemini permission request",
        ]
    }

    fn terminal_prompt_status_line_is_informational(line: &str) -> bool {
        let stripped = Self::strip_ansi_escape_sequences(line);
        let stripped = stripped.trim();
        Self::terminal_prompt_informational_status_prefixes()
            .iter()
            .any(|prefix| {
                Self::ascii_insensitive_suffix_after_prefix(stripped, prefix)
                    .is_some_and(|suffix| suffix.is_empty() || suffix.trim_start().starts_with(':'))
            })
    }

    fn strip_ansi_escape_sequences(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut idx = 0;
        while idx < bytes.len() {
            if bytes[idx] == 0x1b {
                idx += 1;
                if idx >= bytes.len() {
                    break;
                }
                match bytes[idx] {
                    b'[' => {
                        idx += 1;
                        while idx < bytes.len() && !(0x40..=0x7e).contains(&bytes[idx]) {
                            idx += 1;
                        }
                        idx += usize::from(idx < bytes.len());
                    }
                    b']' => {
                        idx += 1;
                        while idx < bytes.len() {
                            if bytes[idx] == 0x07 {
                                idx += 1;
                                break;
                            }
                            if bytes[idx] == 0x1b
                                && idx + 1 < bytes.len()
                                && bytes[idx + 1] == b'\\'
                            {
                                idx += 2;
                                break;
                            }
                            idx += 1;
                        }
                    }
                    _ => idx += 1,
                }
                while idx < bytes.len() && !text.is_char_boundary(idx) {
                    idx += 1;
                }
                continue;
            }
            let ch = text[idx..]
                .chars()
                .next()
                .expect("valid utf-8 char boundary while stripping ANSI");
            out.push(ch);
            idx += ch.len_utf8();
        }
        out
    }

    fn terminal_prompt_excerpt(viewport: &str, signal_line: Option<&str>) -> Option<String> {
        let mut tail = viewport
            .lines()
            .rev()
            .map(str::trim)
            .map(Self::strip_ansi_escape_sequences)
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .take(6)
            .collect::<Vec<_>>();
        tail.reverse();
        if let Some(signal_line) = signal_line
            && !signal_line.is_empty()
        {
            let signal_line = Self::strip_ansi_escape_sequences(signal_line);
            let signal_line = signal_line.trim();
            if !signal_line.is_empty() && !tail.iter().any(|line| line == signal_line) {
                tail.push(signal_line.to_string());
            }
        }
        (!tail.is_empty()).then(|| Self::truncate_blocked_text(&tail.join("\n"), 512))
    }

    fn blocked_info_from_permission_entry(
        pane: &crate::pane::Pane,
        entry: &crate::pane::ActivityEntry,
    ) -> Option<RuntimePaneBlockInfo> {
        if entry.kind != ActivityKind::Permission || entry.status.is_some() {
            return None;
        }
        let action = entry
            .message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())?;
        Some(RuntimePaneBlockInfo {
            kind: RuntimePaneBlockKind::PermissionRequest,
            summary: format!(
                "permission request blocked automatic recovery: {}",
                Self::truncate_blocked_text(action, 160)
            ),
            command_or_tool: Some(Self::truncate_blocked_text(action, 240)),
            request_id: entry.tool_id.clone(),
            task_id: pane.assignment_task_id(),
            excerpt: None,
        })
    }

    fn pane_output_may_contain_blocking_prompt(prefix: &str, data: &[u8]) -> bool {
        Self::terminal_prompt_signal_tokens()
            .iter()
            .chain(Self::terminal_provider_context_limit_signal_tokens().iter())
            .any(|needle| Self::ascii_insensitive_window_contains(prefix, data, needle.as_bytes()))
    }

    fn terminal_prompt_recent_viewport_text(viewport: &str) -> String {
        let mut tail = viewport
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .rev()
            .take(Self::TERMINAL_PROMPT_VIEWPORT_SCAN_LINES)
            .collect::<Vec<_>>();
        tail.reverse();
        tail.join("\n")
    }

    fn blocked_info_from_terminal_prompt_text(
        pane: &crate::pane::Pane,
        text: &str,
        viewport: &str,
    ) -> Option<RuntimePaneBlockInfo> {
        let line = Self::terminal_prompt_signal_line(text)?;
        if Self::ascii_insensitive_starts_with(line, "permission request:") {
            let action = line
                .split_once(':')
                .map(|(_, action)| action.trim())
                .filter(|value| !value.is_empty())
                .map(|value| Self::truncate_blocked_text(value, 240));
            return Some(RuntimePaneBlockInfo {
                kind: RuntimePaneBlockKind::PermissionRequest,
                summary: format!(
                    "permission request blocked automatic recovery: {}",
                    Self::truncate_blocked_text(action.as_deref().unwrap_or(line), 160)
                ),
                command_or_tool: action,
                request_id: None,
                task_id: pane.assignment_task_id(),
                excerpt: Self::terminal_prompt_excerpt(viewport, Some(line)),
            });
        }

        Some(RuntimePaneBlockInfo {
            kind: RuntimePaneBlockKind::TerminalPrompt,
            summary: format!(
                "terminal prompt blocked automatic recovery: {}",
                Self::truncate_blocked_text(line, 160)
            ),
            command_or_tool: Some(Self::truncate_blocked_text(line, 240)),
            request_id: Self::pane_prompt_id_for_blocked(pane),
            task_id: pane.assignment_task_id(),
            excerpt: Self::terminal_prompt_excerpt(viewport, Some(line)),
        })
    }

    fn blocked_info_from_provider_context_limit_text(
        pane: &crate::pane::Pane,
        text: &str,
        viewport: &str,
    ) -> Option<RuntimePaneBlockInfo> {
        let line = Self::terminal_provider_context_limit_line(text)?;
        Some(RuntimePaneBlockInfo {
            kind: RuntimePaneBlockKind::TerminalPrompt,
            summary: format!(
                "provider context limit blocked automatic recovery: {}",
                Self::truncate_blocked_text(line, 160)
            ),
            command_or_tool: Some(Self::truncate_blocked_text(line, 240)),
            request_id: Self::pane_prompt_id_for_blocked(pane),
            task_id: pane.assignment_task_id(),
            excerpt: Self::terminal_prompt_excerpt(viewport, Some(line)),
        })
    }

    fn blocked_info_from_terminal_prompt(
        pane: &crate::pane::Pane,
        prefilter_window: &str,
    ) -> Option<RuntimePaneBlockInfo> {
        let viewport = pane.dump_viewport().ok()?;
        let recent_viewport = Self::terminal_prompt_recent_viewport_text(&viewport);
        Self::blocked_info_from_terminal_prompt_text(pane, &recent_viewport, &viewport)
            .or_else(|| {
                Self::blocked_info_from_provider_context_limit_text(
                    pane,
                    &recent_viewport,
                    &viewport,
                )
            })
            .or_else(|| {
                Self::blocked_info_from_terminal_prompt_text(pane, prefilter_window, &viewport)
            })
            .or_else(|| {
                Self::blocked_info_from_provider_context_limit_text(
                    pane,
                    prefilter_window,
                    &viewport,
                )
            })
    }

    fn permission_resolution_matches_blocked_request(
        pane: &crate::pane::Pane,
        entry: &ActivityEntry,
        now: Instant,
    ) -> bool {
        let resolution_message = entry
            .message
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let blocked_command_matches_resolution = |command_or_tool: Option<&str>| {
            let Some(message) = resolution_message else {
                return false;
            };
            let Some(command_or_tool) = command_or_tool
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return false;
            };
            command_or_tool == message || command_or_tool.ends_with(message)
        };
        let unresolved_fallback_matches = |command_or_tool: Option<&str>| {
            resolution_message.is_none()
                && pane.permission_resolution_fallback_pending(now)
                && command_or_tool
                    .map(str::trim)
                    .is_some_and(|value| !value.is_empty())
        };
        match pane.pane_state() {
            Some(PaneState::Blocked {
                info:
                    RuntimePaneBlockInfo {
                        kind: RuntimePaneBlockKind::PermissionRequest,
                        request_id: Some(blocked_request_id),
                        ..
                    },
                ..
            }) => entry
                .tool_id
                .as_deref()
                .is_some_and(|request_id| blocked_request_id == request_id),
            Some(PaneState::Blocked {
                info:
                    RuntimePaneBlockInfo {
                        kind: RuntimePaneBlockKind::PermissionRequest,
                        request_id: None,
                        command_or_tool,
                        ..
                    },
                ..
            }) => {
                blocked_command_matches_resolution(command_or_tool.as_deref())
                    || unresolved_fallback_matches(command_or_tool.as_deref())
            }
            Some(PaneState::Blocked {
                info:
                    RuntimePaneBlockInfo {
                        kind: RuntimePaneBlockKind::TerminalPrompt,
                        command_or_tool,
                        ..
                    },
                ..
            }) => {
                blocked_command_matches_resolution(command_or_tool.as_deref())
                    || unresolved_fallback_matches(command_or_tool.as_deref())
            }
            _ => false,
        }
    }

    fn refresh_blocked_permission_request(
        pane: &crate::pane::Pane,
        entry: &ActivityEntry,
        now: Instant,
    ) -> Option<RuntimePaneBlockInfo> {
        let blocked = Self::blocked_info_from_permission_entry(pane, entry)?;
        match pane.pane_state() {
            Some(PaneState::Blocked { info, .. })
                if matches!(info.kind, RuntimePaneBlockKind::PermissionRequest)
                    && info.command_or_tool.as_deref() == blocked.command_or_tool.as_deref()
                    && info.request_id.as_deref() != blocked.request_id.as_deref() =>
            {
                Some(blocked)
            }
            Some(PaneState::Blocked { info, .. })
                if matches!(info.kind, RuntimePaneBlockKind::TerminalPrompt)
                    && pane.permission_resolution_fallback_pending(now)
                    && info.request_id.as_deref() != blocked.request_id.as_deref() =>
            {
                Some(blocked)
            }
            _ => None,
        }
    }

    fn mark_pane_blocked(
        &mut self,
        pane_id: &str,
        generation: Generation,
        blocked: RuntimePaneBlockInfo,
        now: Instant,
    ) {
        let mut state_change = None;
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if matches!(
                pane.pane_state(),
                Some(PaneState::Blocked { .. } | PaneState::Dead { .. })
            ) {
                tracing::debug!(
                    pane = %pane_id,
                    blocked = ?blocked,
                    "Skipping mark_pane_blocked: pane already blocked or dead"
                );
                return;
            }
            let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
            pane.set_tool_executing(false);
            pane.set_last_output_at(now);
            pane.set_pane_blocked(blocked.clone(), now);
            state_change =
                Self::runtime_state_change(previous, pane.pane_state(), &blocked.summary);
        }
        self.active_gateway_operations.remove(pane_id);
        super::stability::write_agent_prompt_blocked_marker(pane_id, &blocked);
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
        tracing::warn!(pane = %pane_id, blocked = ?blocked, "Marked pane blocked");
    }

    pub fn mark_gateway_delivery_busy(
        &mut self,
        pane_id: &str,
        prompt_id: PromptId,
        generation: Generation,
        now: Instant,
    ) {
        let mut state_change = None;
        if let Some(pane) = self.panes.get_mut(pane_id) {
            if !matches!(
                pane.pane_state(),
                Some(PaneState::Blocked { .. } | PaneState::Dead { .. })
            ) {
                let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
                pane.set_tool_executing(true);
                if !matches!(pane.pane_state(), Some(PaneState::Busy { .. })) {
                    pane.set_pane_busy(prompt_id.clone(), generation, now);
                }
                state_change = Self::runtime_state_change(
                    previous,
                    pane.pane_state(),
                    "gateway reported active prompt",
                );
            }
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
        tracing::warn!(
            pane = %pane_id,
            prompt_id = %prompt_id,
            generation = generation.0,
            "Gateway reported an active prompt while mux had no live turn; marked pane busy"
        );
    }

    fn accept_generation_event(&self, pane_id: &str, event_gen: Generation) -> bool {
        let Some(pane_gen) = self
            .panes
            .get(pane_id)
            .map(|pane| pane.current_generation())
        else {
            tracing::debug!(
                pane_id = %pane_id,
                event_gen = event_gen.0,
                "dropped stale event for old generation"
            );
            return false;
        };

        if event_gen != pane_gen {
            tracing::debug!(
                pane_id = %pane_id,
                event_gen = event_gen.0,
                pane_gen = pane_gen.0,
                "dropped stale event for old generation"
            );
            return false;
        }

        true
    }

    fn apply_busy_ready_transition(
        pane: &mut crate::pane::Pane,
        pane_id: &str,
        generation: Generation,
        now: Instant,
        busy: bool,
        synthetic_prompt_prefix: &str,
        completed_operation: bool,
    ) -> Option<(
        Option<RuntimePaneState>,
        RuntimePaneState,
        String,
        Option<RuntimePaneBlockInfo>,
    )> {
        let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
        if busy {
            match pane.pane_state() {
                Some(PaneState::Busy {
                    generation: busy_generation,
                    ..
                }) if *busy_generation == generation => {
                    pane.touch_busy_activity(now);
                }
                _ => {
                    pane.set_pane_busy(
                        Self::synthetic_busy_prompt_id(synthetic_prompt_prefix, pane_id),
                        generation,
                        now,
                    );
                }
            }
            return Self::runtime_state_change(previous, pane.pane_state(), "activity started");
        }

        if completed_operation && pane.state_machine_operation_completed(generation, now) {
            return Self::runtime_state_change(previous, pane.pane_state(), "activity completed");
        }

        pane.set_pane_ready(now);
        Self::runtime_state_change(previous, pane.pane_state(), "activity ready")
    }

    fn apply_queued_event(&mut self, event: &MuxEvent) -> bool {
        match event {
            MuxEvent::PaneOutput {
                pane_id,
                data,
                generation,
            } => {
                if !self.accept_generation_event(pane_id, *generation) {
                    return false;
                }
                let now = Instant::now();
                let mut blocked = None;
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    if let Err(err) = pane.append_output(data) {
                        tracing::warn!(
                            pane = %pane_id,
                            error = %err,
                            "Failed to append queued pane output"
                        );
                    } else if !matches!(
                        pane.pane_state(),
                        Some(PaneState::Blocked { .. } | PaneState::Dead { .. })
                    ) {
                        let needs_prompt_scan = Self::pane_output_may_contain_blocking_prompt(
                            &pane.terminal_prompt_prefilter_tail,
                            data,
                        );
                        if needs_prompt_scan {
                            let prefilter_window =
                                Self::terminal_prompt_prefilter_window(pane, data);
                            blocked =
                                Self::blocked_info_from_terminal_prompt(pane, &prefilter_window);
                            pane.terminal_prompt_prefilter_tail =
                                Self::terminal_prompt_prefilter_tail(&prefilter_window);
                        } else {
                            Self::update_terminal_prompt_prefilter_tail(
                                &mut pane.terminal_prompt_prefilter_tail,
                                data,
                            );
                        }
                    } else {
                        Self::update_terminal_prompt_prefilter_tail(
                            &mut pane.terminal_prompt_prefilter_tail,
                            data,
                        );
                    }
                }
                if let Some(blocked) = blocked {
                    self.mark_pane_blocked(pane_id, *generation, blocked, now);
                }
                true
            }
            MuxEvent::ActivityEvent {
                pane_id,
                entry,
                generation,
            } => {
                if !self.accept_generation_event(pane_id, *generation) {
                    return false;
                }
                tracing::trace!(
                    pane = %pane_id,
                    generation = generation.0,
                    kind = ?entry.kind,
                    status = ?entry.status.as_deref(),
                    "Applying activity event"
                );
                let permission_resolved = matches!(
                    (entry.kind, entry.status.as_deref()),
                    (
                        ActivityKind::Permission,
                        Some("approved" | "denied" | "resolved")
                    )
                );
                let now = Instant::now();
                let permission_resolution_matches_blocked = permission_resolved
                    && self.panes.get(pane_id).is_some_and(|pane| {
                        Self::permission_resolution_matches_blocked_request(pane, entry, now)
                    });
                let operations_active = match entry.kind {
                    ActivityKind::Operation => {
                        let count = self
                            .active_gateway_operations
                            .entry(pane_id.clone())
                            .or_insert(0);
                        match entry.status.as_deref() {
                            Some("started") => {
                                *count = count.saturating_add(1);
                            }
                            Some("completed" | "failed" | "cancelled" | "success" | "ok") => {
                                *count = count.saturating_sub(1);
                            }
                            _ => {}
                        }
                        let active = *count > 0;
                        if !active {
                            self.active_gateway_operations.remove(pane_id);
                        }
                        active
                    }
                    _ => {
                        self.active_gateway_operations
                            .get(pane_id)
                            .copied()
                            .unwrap_or(0)
                            > 0
                    }
                };
                let mut state_change = None;
                let mut permission_blocked = None;
                let mut refreshed_blocked_permission = None;
                let mut blocked_refresh_event = None;
                let mut clear_prompt_blocked_health = false;
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    if !matches!(
                        pane.pane_state(),
                        Some(PaneState::Blocked { .. } | PaneState::Dead { .. })
                    ) {
                        permission_blocked = Self::blocked_info_from_permission_entry(pane, entry);
                    } else {
                        refreshed_blocked_permission =
                            Self::refresh_blocked_permission_request(pane, entry, now);
                        if let Some(blocked) = refreshed_blocked_permission.as_ref() {
                            pane.refresh_blocked_info(blocked.clone(), now);
                            blocked_refresh_event = Some(blocked.clone());
                        } else if pane.permission_resolution_fallback_expired(now)
                            && !permission_resolution_matches_blocked
                        {
                            pane.clear_permission_resolution_fallback();
                        }
                    }
                    pane.record_output_activity();
                    pane.ensure_activity_buffer();
                    let mut tools_active = false;
                    if let Some(buf) = pane.activity_buffer_mut() {
                        match entry.kind {
                            ActivityKind::ToolCall => {
                                if let (Some(tool_id), Some(tool_name)) =
                                    (&entry.tool_id, &entry.tool_name)
                                {
                                    let status = entry.status.as_deref();
                                    if matches!(status, Some("started")) {
                                        let duplicate_start = buf.active_tool(tool_id).is_some();
                                        buf.start_tool(tool_id.clone(), tool_name.clone());
                                        if !duplicate_start {
                                            buf.flush_output_buffer();
                                            buf.push(entry.clone());
                                        }
                                    } else {
                                        let duration = buf.complete_tool(tool_id).map(|active| {
                                            std::time::Instant::now()
                                                .duration_since(active.started_at)
                                        });
                                        buf.flush_output_buffer();
                                        let mut completed_entry = entry.clone();
                                        completed_entry.duration = duration;
                                        buf.push(completed_entry);
                                    }
                                }
                                tools_active = buf.has_in_flight_tools();
                            }
                            ActivityKind::Output => {
                                if let Some(chunks) = &entry.output_chunks {
                                    for chunk in chunks {
                                        buf.append_output(chunk);
                                    }
                                }
                                tools_active = buf.has_in_flight_tools();
                            }
                            _ => {
                                buf.flush_output_buffer();
                                buf.push(entry.clone());
                                tools_active = buf.has_in_flight_tools();
                            }
                        }
                    }
                    if permission_blocked.is_none() {
                        if permission_resolution_matches_blocked {
                            let previous =
                                pane.pane_state().map(Self::runtime_pane_state_for_state);
                            pane.restore_after_blocked_permission_resolution(
                                Self::synthetic_busy_prompt_id("permission-resolved", pane_id),
                                *generation,
                                now,
                            );
                            state_change = Self::runtime_state_change(
                                previous,
                                pane.pane_state(),
                                "permission resolved",
                            );
                            clear_prompt_blocked_health = true;
                        } else {
                            let busy = operations_active || tools_active;
                            pane.set_tool_executing(busy);
                            let completed = matches!(
                                (entry.kind, entry.status.as_deref()),
                                (
                                    ActivityKind::Operation,
                                    Some("completed" | "failed" | "cancelled" | "success" | "ok")
                                )
                            );
                            state_change = Self::apply_busy_ready_transition(
                                pane,
                                pane_id,
                                *generation,
                                now,
                                busy,
                                "activity",
                                completed,
                            );
                        }
                    }
                }
                if let Some((previous, current, reason, blocked)) = state_change {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        *generation,
                        previous,
                        current,
                        Some(reason),
                        blocked,
                    );
                }
                if let Some(blocked) = blocked_refresh_event {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        *generation,
                        Some(RuntimePaneState::Blocked),
                        RuntimePaneState::Blocked,
                        Some("permission request details refreshed".to_string()),
                        Some(blocked),
                    );
                }
                if clear_prompt_blocked_health {
                    super::stability::clear_agent_health_marker(pane_id);
                }
                if let Some(blocked) = refreshed_blocked_permission {
                    super::stability::write_agent_prompt_blocked_marker(pane_id, &blocked);
                }
                if let Some(blocked) = permission_blocked {
                    self.mark_pane_blocked(pane_id, *generation, blocked, now);
                }
                true
            }
            MuxEvent::AsyncGatewayPromptDeliveryCompleted {
                pane_id,
                prompt,
                from,
                generation,
                result,
            } => {
                if !self.accept_generation_event(pane_id, *generation) {
                    return false;
                }
                let now = Instant::now();
                let mut state_change = None;
                match result {
                    Ok(PromptDeliveryAttempt::Delivered {
                        prompt_id,
                        generation: delivered_generation,
                    }) => {
                        if let Some(pane) = self.panes.get_mut(pane_id) {
                            let previous =
                                pane.pane_state().map(Self::runtime_pane_state_for_state);
                            pane.set_last_output_at(now);
                            pane.set_tool_executing(true);
                            pane.set_pane_busy(prompt_id.clone(), *delivered_generation, now);
                            state_change = Self::runtime_state_change(
                                previous,
                                pane.pane_state(),
                                "prompt delivered",
                            );
                        }
                        self.finalize_async_gateway_prompt_delivery(
                            pane_id,
                            prompt,
                            from.as_deref(),
                            Ok(()),
                        )
                    }
                    Ok(PromptDeliveryAttempt::Queued {
                        prompt_id,
                        ahead_of,
                    }) => {
                        self.mark_gateway_delivery_busy(
                            pane_id,
                            prompt_id.clone(),
                            *generation,
                            now,
                        );
                        let inject_after = now + GATEWAY_PROMPT_RETRY_DELAY;
                        match self.queue_delayed_prompt(
                            pane_id,
                            prompt.clone(),
                            from.clone(),
                            inject_after,
                            Some(prompt_id.clone()),
                        ) {
                            PromptDeliveryAttempt::Queued { .. } => {
                                tracing::info!(
                                    pane = %pane_id,
                                    prompt_id = %prompt_id,
                                    ahead_of,
                                    deliver_after_ms = %GATEWAY_PROMPT_RETRY_DELAY.as_millis(),
                                    "Queued async gateway prompt delivery while the agent turn is active"
                                );
                            }
                            PromptDeliveryAttempt::AlreadyPresent { position, .. } => {
                                tracing::debug!(
                                    pane = %pane_id,
                                    prompt_id = %prompt_id,
                                    position = %position,
                                    "Async gateway prompt already present in retry queue"
                                );
                            }
                            PromptDeliveryAttempt::Rejected { reason } => {
                                tracing::warn!(
                                    pane = %pane_id,
                                    prompt_id = %prompt_id,
                                    reason = ?reason,
                                    "Rejected async gateway prompt queueing"
                                );
                            }
                            PromptDeliveryAttempt::Delivered { .. } => {}
                        }
                    }
                    Ok(PromptDeliveryAttempt::AlreadyPresent {
                        prompt_id,
                        position,
                    }) => {
                        tracing::debug!(
                            pane = %pane_id,
                            prompt_id = %prompt_id,
                            position = %position,
                            "Async gateway prompt completion reported an already-present prompt"
                        );
                    }
                    Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                        self.finalize_async_gateway_prompt_delivery(
                            pane_id,
                            prompt,
                            from.as_deref(),
                            Err(super::types::AsyncGatewayPromptDeliveryError {
                                error: format!("prompt delivery rejected: {reason:?}"),
                            }),
                        );
                    }
                    Err(err) => {
                        if Self::is_busy_gateway_delivery_error(&err.error) {
                            let inject_after = now + GATEWAY_PROMPT_RETRY_DELAY;
                            let retry_prompt_id =
                                brehon_types::PromptId::new(uuid::Uuid::new_v4().to_string());
                            match self.queue_delayed_prompt(
                                pane_id,
                                prompt.clone(),
                                from.clone(),
                                inject_after,
                                Some(retry_prompt_id.clone()),
                            ) {
                                PromptDeliveryAttempt::Queued { .. } => {
                                    tracing::info!(
                                        pane = %pane_id,
                                        prompt_id = %retry_prompt_id,
                                        deliver_after_ms = %GATEWAY_PROMPT_RETRY_DELAY.as_millis(),
                                        "Queued async gateway prompt delivery because a prompt is already in progress"
                                    );
                                }
                                PromptDeliveryAttempt::AlreadyPresent { position, .. } => {
                                    tracing::debug!(
                                        pane = %pane_id,
                                        prompt_id = %retry_prompt_id,
                                        position = %position,
                                        "Busy async gateway prompt was already queued"
                                    );
                                }
                                PromptDeliveryAttempt::Rejected { reason } => {
                                    tracing::warn!(
                                        pane = %pane_id,
                                        prompt_id = %retry_prompt_id,
                                        reason = ?reason,
                                        "Rejected busy async gateway prompt queueing"
                                    );
                                }
                                PromptDeliveryAttempt::Delivered { .. } => {}
                            }
                            return true;
                        }
                        self.finalize_async_gateway_prompt_delivery(
                            pane_id,
                            prompt,
                            from.as_deref(),
                            Err(err.clone()),
                        );
                    }
                }
                if let Some((previous, current, reason, blocked)) = state_change {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        *generation,
                        previous,
                        current,
                        Some(reason),
                        blocked,
                    );
                }
                true
            }
            MuxEvent::AsyncTeamsPromptDeliveryCompleted {
                pane_id,
                team,
                generation,
                result,
            } => {
                if !self.accept_generation_event(pane_id, *generation) {
                    return false;
                }
                match result {
                    Ok(PromptDeliveryAttempt::Delivered { .. }) => {}
                    Ok(PromptDeliveryAttempt::Queued {
                        prompt_id,
                        ahead_of,
                    }) => {
                        tracing::info!(
                            pane = %pane_id,
                            prompt_id = %prompt_id,
                            ahead_of,
                            "Queued async Teams inbox prompt delivery"
                        );
                    }
                    Ok(PromptDeliveryAttempt::AlreadyPresent {
                        prompt_id,
                        position,
                    }) => {
                        tracing::debug!(
                            pane = %pane_id,
                            prompt_id = %prompt_id,
                            position = %position,
                            "Async Teams inbox prompt already present"
                        );
                    }
                    Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                        let error = format!("prompt delivery rejected: {reason:?}");
                        Self::log_teams_inbox_delivery_failure(team, pane_id, &error);
                        self.finalize_async_teams_prompt_delivery(
                            pane_id,
                            Err(super::types::AsyncGatewayPromptDeliveryError { error }),
                        );
                    }
                    Err(err) => {
                        Self::log_teams_inbox_delivery_failure(team, pane_id, &err.error);
                        self.finalize_async_teams_prompt_delivery(pane_id, Err(err.clone()));
                    }
                }
                true
            }
            MuxEvent::ActivityFlush {
                pane_id,
                generation,
            } => {
                if !self.accept_generation_event(pane_id, *generation) {
                    return false;
                }
                let mut state_change = None;
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.record_output_activity();
                    let now = Instant::now();
                    let mut tools_active = false;
                    if let Some(buf) = pane.activity_buffer_mut() {
                        buf.finalize();
                        tools_active = buf.has_in_flight_tools();
                    }
                    let operations_active = self
                        .active_gateway_operations
                        .get(pane_id)
                        .copied()
                        .unwrap_or(0)
                        > 0;
                    let busy = operations_active || tools_active;
                    pane.set_tool_executing(busy);
                    state_change = Self::apply_busy_ready_transition(
                        pane,
                        pane_id,
                        *generation,
                        now,
                        busy,
                        "flush",
                        false,
                    );
                }
                if let Some((previous, current, reason, blocked)) = state_change {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        *generation,
                        previous,
                        current,
                        Some(reason),
                        blocked,
                    );
                }
                true
            }
            MuxEvent::TaskContextChanged { pane_id, context } => {
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    match context {
                        Some(ctx) => pane.set_task_context(ctx.clone()),
                        None => pane.clear_task_context(),
                    }
                }
                true
            }
            MuxEvent::ReviewContextChanged { pane_id, context } => {
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    match context {
                        Some(ctx) => pane.set_review_context(ctx.clone()),
                        None => pane.clear_review_context(),
                    }
                }
                true
            }
            _ => true,
        }
    }

    /// Tick the pane state machine and dispatch at most one queued prompt per
    /// pane that is currently `Ready`.
    ///
    /// Busy → Ready transitions are applied via `Pane::tick_state_machine`
    /// before dispatch selection.
    pub fn tick_pane_state_machine(&mut self, rt: &tokio::runtime::Handle) {
        self.tick_pane_state_machine_at(rt, Instant::now());
    }

    /// Advance the pane state machine using a caller-provided clock instant.
    ///
    /// Primarily intended for deterministic test harnesses that need to drive
    /// delayed startup-prompt delivery and Busy → Ready transitions without
    /// sleeping in wall-clock time.
    pub fn tick_pane_state_machine_at(&mut self, rt: &tokio::runtime::Handle, now: Instant) {
        self.tick_pane_state_machine_impl(rt, now);
    }

    fn tick_pane_state_machine_impl(&mut self, rt: &tokio::runtime::Handle, now: Instant) {
        let pane_ids: Vec<String> = self.panes.keys().cloned().collect();
        for pane_id in pane_ids {
            let state_change = if let Some(pane) = self.panes.get_mut(&pane_id) {
                let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
                if pane.tick_state_machine(now) {
                    pane.set_tool_executing(false);
                    Self::runtime_state_change(previous, pane.pane_state(), "state machine ready")
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((previous, current, reason, blocked)) = state_change {
                let generation = self.current_generation_or_default(&pane_id);
                self.publish_runtime_pane_state_changed(
                    &pane_id,
                    generation,
                    previous,
                    current,
                    Some(reason),
                    blocked,
                );
            }

            let pane_ready = self.panes.get(&pane_id).is_some_and(|pane| {
                matches!(pane.pane_state(), None | Some(PaneState::Ready { .. }))
            });
            if pane_ready {
                self.try_dispatch_next_ready_queued_prompt(rt, &pane_id, now);
            }
        }
    }

    fn take_next_ready_queued_prompt(
        &mut self,
        pane_id: &str,
        now: Instant,
    ) -> Option<QueuedPrompt> {
        let pane = self.panes.get_mut(pane_id)?;
        if let Some(queued) = pane.take_ready_delayed_prompt(now) {
            return Some(queued);
        }

        pane.promote_waiting_delayed_prompt();
        pane.take_ready_delayed_prompt(now)
    }

    fn requeue_prompt_at_front(
        &mut self,
        pane_id: &str,
        prompt_id: PromptId,
        prompt: String,
        from: Option<String>,
        generation: Generation,
        inject_after: Instant,
    ) {
        if let Some(pane) = self.panes.get_mut(pane_id) {
            let _ = pane.enqueue_delayed_prompt(
                QueuedPrompt {
                    prompt_id,
                    prompt,
                    from,
                    inject_after,
                    generation,
                },
                DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP,
            );
        }
    }

    fn mark_busy_after_queue_dispatch(
        &mut self,
        pane_id: &str,
        prompt_id: PromptId,
        generation: Generation,
        now: Instant,
    ) {
        let mut state_change = None;
        if let Some(pane) = self.panes.get_mut(pane_id) {
            let previous = pane.pane_state().map(Self::runtime_pane_state_for_state);
            pane.set_last_output_at(now);
            pane.set_tool_executing(true);
            pane.set_pane_busy(prompt_id, generation, now);
            state_change =
                Self::runtime_state_change(previous, pane.pane_state(), "queued prompt dispatched");
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
    }

    fn try_dispatch_next_ready_queued_prompt(
        &mut self,
        rt: &tokio::runtime::Handle,
        pane_id: &str,
        now: Instant,
    ) {
        let Some(queued) = self.take_next_ready_queued_prompt(pane_id, now) else {
            return;
        };
        let QueuedPrompt {
            prompt_id,
            prompt,
            from,
            generation,
            ..
        } = queued;

        match rt.block_on(self.attempt_prompt_delivery(pane_id, &prompt, from.as_deref())) {
            Ok(PromptDeliveryAttempt::Delivered {
                prompt_id,
                generation,
            }) => {
                self.mark_busy_after_queue_dispatch(pane_id, prompt_id, generation, now);
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.promote_waiting_delayed_prompt();
                }
            }
            Ok(PromptDeliveryAttempt::Queued { .. }) => {
                let inject_after = now + GATEWAY_PROMPT_RETRY_DELAY;
                self.requeue_prompt_at_front(
                    pane_id,
                    prompt_id,
                    prompt,
                    from,
                    generation,
                    inject_after,
                );
            }
            Ok(PromptDeliveryAttempt::AlreadyPresent { .. }) => {
                tracing::debug!(
                    pane = %pane_id,
                    prompt_id = %prompt_id,
                    generation = generation.0,
                    "Queued prompt already present during state-machine dispatch"
                );
            }
            Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                tracing::warn!(
                    pane = %pane_id,
                    prompt_id = %prompt_id,
                    generation = generation.0,
                    reason = ?reason,
                    "Queued prompt rejected during state-machine dispatch"
                );
            }
            Err(err) => {
                tracing::warn!(
                    pane = %pane_id,
                    prompt_id = %prompt_id,
                    generation = generation.0,
                    error = %err,
                    "Failed to dispatch queued prompt on state-machine tick; requeueing"
                );
                let inject_after = now + GATEWAY_PROMPT_RETRY_DELAY;
                self.requeue_prompt_at_front(
                    pane_id,
                    prompt_id,
                    prompt,
                    from,
                    generation,
                    inject_after,
                );
            }
        }
    }

    pub(super) fn spawn_acp_event_bridge(
        &self,
        pane_id: &str,
        mut rx: mpsc::Receiver<brehon_acp::updates::SessionEvent>,
    ) {
        let pane_id_str = pane_id.to_string();
        let generation = self
            .panes
            .get(pane_id)
            .map(|pane| pane.current_generation())
            .unwrap_or_else(|| {
                tracing::warn!(
                    pane = %pane_id,
                    "Missing pane while spawning ACP event bridge; defaulting generation to 0"
                );
                Generation::default()
            });
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let mut active_tool_names: HashMap<String, String> = HashMap::new();
            let mut last_event_was_output = false;
            let mut last_output_ended_with_newline = true;
            while let Some(event) = rx.recv().await {
                let (event, suppress_formatted_output) =
                    normalize_gateway_tool_event(event, &mut active_tool_names);
                let is_output = matches!(event, brehon_acp::updates::SessionEvent::Output { .. });
                let output_ended_with_newline = match &event {
                    brehon_acp::updates::SessionEvent::Output { text, .. } => {
                        text.ends_with('\n') || text.ends_with('\r')
                    }
                    _ => false,
                };

                if let Some(entry) = session_event_to_activity_entry(&event) {
                    let _ = event_tx
                        .send(MuxEvent::ActivityEvent {
                            pane_id: pane_id_str.clone(),
                            entry,
                            generation,
                        })
                        .await;
                }

                if !suppress_formatted_output
                    && let Some(mut data) = format_acp_session_event(&event)
                {
                    if Self::should_prefix_output_after_hidden_boundary(
                        &event,
                        &data,
                        last_event_was_output,
                        last_output_ended_with_newline,
                    ) {
                        let mut prefixed = Vec::with_capacity(data.len() + 2);
                        prefixed.extend_from_slice(b"\r\n");
                        prefixed.extend_from_slice(&data);
                        data = prefixed;
                    }
                    let _ = event_tx
                        .send(MuxEvent::PaneOutput {
                            pane_id: pane_id_str.clone(),
                            data,
                            generation,
                        })
                        .await;
                }

                if is_output {
                    last_event_was_output = true;
                    last_output_ended_with_newline = output_ended_with_newline;
                } else {
                    last_event_was_output = false;
                }
            }

            let _ = event_tx
                .send(MuxEvent::ActivityFlush {
                    pane_id: pane_id_str.clone(),
                    generation,
                })
                .await;
        });
    }

    pub(crate) fn should_prefix_output_after_hidden_boundary(
        event: &brehon_acp::updates::SessionEvent,
        data: &[u8],
        last_event_was_output: bool,
        last_output_ended_with_newline: bool,
    ) -> bool {
        matches!(event, brehon_acp::updates::SessionEvent::Output { .. })
            && !last_event_was_output
            && !last_output_ended_with_newline
            && !data.starts_with(b"\r")
            && !data.starts_with(b"\n")
    }

    /// Poll all panes for events (non-blocking)
    ///
    /// Returns one event at a time. Call in a loop until None to process all events.
    pub fn poll(&mut self) -> Option<MuxEvent> {
        // First check the event queue
        while let Ok(event) = self.event_rx.try_recv() {
            if self.apply_queued_event(&event) {
                self.publish_runtime_event_for_mux_event(&event);
                return Some(event);
            }
        }

        if self.pending_panesmith_events.is_empty() {
            let events = self.drain_panesmith_events_to_mux();
            self.pending_panesmith_events.extend(events);
        }
        if let Some(event) = self.pending_panesmith_events.pop_front() {
            self.publish_runtime_event_for_mux_event(&event);
            return Some(event);
        }

        // Poll each pane for ONE event (not draining all)
        // This ensures we return events as they come without losing any
        let mut next_event = None;
        for (id, pane) in self.panes.iter_mut() {
            if let Some(event) = pane.poll() {
                match event {
                    PtyEvent::Output(data) => {
                        let generation = pane.current_generation();
                        next_event = Some(MuxEvent::PaneOutput {
                            pane_id: id.clone(),
                            data,
                            generation,
                        });
                        break;
                    }
                    PtyEvent::CursorPositionRequested => {
                        // Already handled inside Pane::poll — it wrote the
                        // CPR reply directly to the PTY. Nothing to surface
                        // at the mux layer; keep polling for real events.
                    }
                    PtyEvent::Exited(code) => {
                        next_event = Some(MuxEvent::PaneExited {
                            pane_id: id.clone(),
                            exit_code: code,
                        });
                        break;
                    }
                    PtyEvent::Error(e) => {
                        tracing::error!("PTY error in pane {}: {}", id, e);
                        // Continue to next pane/event
                    }
                }
            }
        }

        if let Some(event) = &next_event {
            self.publish_runtime_event_for_mux_event(event);
        }

        next_event
    }

    /// Poll all panes and drain all available events at once (more efficient for multi-pane)
    ///
    /// Returns (total_bytes, events). Uses coalesced output feeding for efficiency when
    /// multiple Claude instances are generating long responses simultaneously.
    ///
    /// The MuxEvent::PaneOutput events include the raw PTY bytes so WebSocket clients
    /// can feed them to their own terminal emulators.
    pub fn poll_batch(&mut self) -> (usize, Vec<MuxEvent>) {
        self.poll_batch_inner(None)
    }

    /// Poll all panes while only refreshing owned Panesmith snapshots for the
    /// panes the caller may render immediately.
    pub fn poll_batch_with_panesmith_snapshot_panes(
        &mut self,
        snapshot_panes: &BTreeSet<String>,
    ) -> (usize, Vec<MuxEvent>) {
        self.poll_batch_inner(Some(snapshot_panes))
    }

    fn poll_batch_inner(
        &mut self,
        snapshot_panes: Option<&BTreeSet<String>>,
    ) -> (usize, Vec<MuxEvent>) {
        let mut events = Vec::new();
        let mut total_bytes = 0;

        // First drain the event queue
        for _ in 0..self.max_queued_events_per_poll {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            if !self.apply_queued_event(&event) {
                continue;
            }
            if let MuxEvent::PaneOutput { data, .. } = &event {
                total_bytes += data.len();
            }
            events.push(event);
        }

        while let Some(event) = self.pending_panesmith_events.pop_front() {
            events.push(event);
        }
        match snapshot_panes {
            Some(snapshot_panes) => {
                events.extend(self.drain_panesmith_events_to_mux_for_snapshots(snapshot_panes));
            }
            None => events.extend(self.drain_panesmith_events_to_mux()),
        }

        // Drain each pane using coalesced output (more efficient for high throughput)
        for (id, pane) in self.panes.iter_mut() {
            let (data, other_events) = pane.drain_output();
            let generation = pane.current_generation();

            if !data.is_empty() {
                total_bytes += data.len();
                // Include raw data for WebSocket clients
                events.push(MuxEvent::PaneOutput {
                    pane_id: id.clone(),
                    data,
                    generation,
                });
            }

            // Forward non-output events
            for event in other_events {
                match event {
                    PtyEvent::Exited(code) => {
                        events.push(MuxEvent::PaneExited {
                            pane_id: id.clone(),
                            exit_code: code,
                        });
                    }
                    PtyEvent::Error(e) => {
                        tracing::error!("PTY error in pane {}: {}", id, e);
                    }
                    _ => {}
                }
            }
        }

        for event in &events {
            self.publish_runtime_event_for_mux_event(event);
        }

        (total_bytes, events)
    }

    /// Receive the next event (blocking)
    pub async fn recv(&mut self) -> Option<MuxEvent> {
        self.event_rx.recv().await
    }
}
