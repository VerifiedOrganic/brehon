//! Terminal state management, viewport rendering, output filtering, and styling.

use crate::error::{Error, Result};
use crate::pane::activity::ActivityBuffer;
use crate::pane::spawn::spawn_config_for_pty_spawn;
use crate::pane::style::{cell_style_to_ratatui, debug_log_enabled, styles_equal};
use crate::pane::types::{
    BufferedMessage, ClaudePromptState, GatewaySpawnConfig, Pane, PaneBackend, PaneKind,
    ReviewContextSnapshot, TaskContextSnapshot,
};
use crate::pane::{DeathReason, Generation, PaneState};
use crate::pty::{Pty, PtyConfig};
use ghostty_vt::CellStyle;
use ghostty_vt::{Rgb, Terminal};
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const CLAUDE_TEAMS_STARTUP_SETTLE_DELAY: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayViewportRow {
    pub(crate) source_row: u16,
    pub(crate) text: String,
}

impl Pane {
    fn rebuild_terminal_state(&mut self) -> Result<()> {
        let terminal =
            Terminal::new(self.rows, self.cols).map_err(|e| Error::terminal(e.to_string()))?;
        terminal.set_default_colors(Rgb { r: 0, g: 0, b: 0 }, Rgb { r: 0, g: 0, b: 0 });
        let info = terminal.scrollback_info();
        self.terminal = terminal;
        self.force_all_dirty = true;
        self.render_generation = self.render_generation.wrapping_add(1);
        self.last_total_scrollback = info.total_scrollback;
        self.seq_counter = 0;
        self.synthetic_prev_was_cr = false;
        self.supervisor_pending_structured_output.clear();
        self.pending_messages.clear();
        *self.pending_ink_submit.lock() = None;
        self.ink_submit_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn restart_pty_from_spawn_config(&mut self) -> Result<()> {
        self.spawn_pty_from_spawn_config(true)
    }

    pub(crate) fn start_pty_from_spawn_config(&mut self) -> Result<()> {
        self.spawn_pty_from_spawn_config(false)
    }

    fn spawn_pty_from_spawn_config(&mut self, refresh_session_id: bool) -> Result<()> {
        let mut config = self
            .pty_spawn_config
            .clone()
            .ok_or_else(|| Error::pty("Pane has no stored PTY spawn config"))?;
        if refresh_session_id {
            refresh_brehon_session_id(&mut config);
        }

        let new_session_id = config
            .env
            .iter()
            .find_map(|(key, value)| (key == "BREHON_SESSION_ID").then(|| value.clone()));

        if let PaneBackend::Pty(pty) = &mut self.backend {
            pty.kill();
        }

        let spawn_config = spawn_config_for_pty_spawn(&config);
        let pty = Pty::spawn(self.id.clone(), spawn_config)?;
        self.backend = PaneBackend::Pty(pty);
        self.pty_spawn_config = Some(config);
        self.agent_session_id = new_session_id;
        self.exited = false;
        self.exit_code = None;
        self.pending_inbox_nudge = false;
        self.pending_inbox_nudge_since = None;
        self.last_output_at = Instant::now();
        self.set_tool_executing(false);
        self.arm_claude_inbox_nudge_grace_period();
        self.rebuild_terminal_state()?;
        Ok(())
    }

    /// Get the unique pane identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the terminal column count.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Get the terminal row count.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Monotonic counter that advances whenever this pane's visible state
    /// could have changed (parser fed bytes, resize, scroll, synthetic
    /// append, etc.). The TUI render widget caches viewport row data
    /// keyed on this counter so an idle pane skips the per-row FFI
    /// round-trip on every frame. Distinct from `take_dirty_rows`, which
    /// is consumed destructively by the WebSocket snapshot path in
    /// `snapshot.rs`.
    pub fn render_generation(&self) -> u64 {
        self.render_generation
    }

    /// Advance the render generation. Idempotent and cheap; the only
    /// observable effect is that the next render frame re-fetches its
    /// cached row data from ghostty_vt.
    pub(crate) fn bump_render_generation(&mut self) {
        self.render_generation = self.render_generation.wrapping_add(1);
    }

    /// Whether this pane's PTY and terminal surface are managed by Panesmith.
    pub fn is_panesmith_managed(&self) -> bool {
        self.panesmith_managed
    }

    /// Mark this pane as using Panesmith for its PTY/surface path.
    pub(crate) fn set_panesmith_managed(&mut self, managed: bool) {
        self.panesmith_managed = managed;
    }

    /// Get the kind of this pane (worker, supervisor, director, etc.).
    pub fn kind(&self) -> &PaneKind {
        &self.kind
    }

    /// Get the display title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Set the display title.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    /// Get the border color (hex string), if set.
    pub fn color(&self) -> Option<&str> {
        self.color.as_deref()
    }

    /// Set the border color (hex string).
    pub fn set_color(&mut self, color: impl Into<String>) {
        self.color = Some(color.into());
    }

    /// Whether this pane currently has focus.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Set the focus state of this pane.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Mark all rows as dirty so the next update sends a full refresh.
    pub fn mark_all_dirty(&mut self) {
        self.force_all_dirty = true;
    }

    pub(crate) fn take_force_all_dirty(&mut self) -> bool {
        std::mem::take(&mut self.force_all_dirty)
    }

    /// Whether the pane's backing process has exited.
    pub fn has_exited(&self) -> bool {
        self.exited
    }

    /// Get the exit code if the process has exited.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Get the terminal dimensions as `(rows, cols)`.
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Get the agent adapter running in this pane.
    pub fn cli_type(&self) -> &crate::harness::AgentAdapter {
        &self.cli_type
    }

    /// Get the configured agent type alias, if different from the built-in CLI name.
    pub fn configured_agent_type(&self) -> Option<&str> {
        self.configured_agent_type.as_deref()
    }

    /// Get the stable BREHON_SESSION_ID assigned at spawn time, if any.
    pub fn agent_session_id(&self) -> Option<&str> {
        self.agent_session_id.as_deref()
    }

    /// Returns the ACP gateway session ID if this pane's agent was spawned
    /// through the `AgentGateway` (piped stdio, not PTY).
    pub fn gateway_session_id(&self) -> Option<&str> {
        self.gateway_session_id.as_deref()
    }

    /// Set the ACP gateway session ID for this pane.
    ///
    /// This compatibility setter routes through generation-safe spawn
    /// registration so callers cannot attach a session without advancing the
    /// ACP generation counter.
    pub fn set_gateway_session_id(&mut self, id: String) {
        self.register_gateway_session_spawn(id);
    }

    /// Record a freshly spawned ACP gateway session and advance generation.
    pub(crate) fn register_gateway_session_spawn(&mut self, id: String) {
        self.gateway_session_id = Some(id);
        self.current_generation = Generation(self.current_generation.0.saturating_add(1));
    }

    /// Clear the gateway session and terminal IDs (e.g., on delivery failure).
    pub fn clear_gateway_session(&mut self) {
        self.gateway_session_id = None;
        self.gateway_terminal_id = None;
    }

    /// Current ACP session generation for this pane.
    pub fn current_generation(&self) -> Generation {
        self.current_generation
    }

    /// Get the gateway spawn config, if this pane was created for gateway transport.
    pub(crate) fn gateway_spawn_config(&self) -> Option<&GatewaySpawnConfig> {
        self.gateway_spawn_config.as_ref()
    }

    /// Take the gateway spawn config, consuming it from this pane.
    #[allow(dead_code)] // WIP: used by upcoming gateway spawn logic
    pub(crate) fn take_gateway_spawn_config(&mut self) -> Option<GatewaySpawnConfig> {
        self.gateway_spawn_config.take()
    }

    /// Restore a previously taken gateway spawn config (e.g., after a failed spawn).
    #[allow(dead_code)] // WIP: used by upcoming gateway spawn logic
    pub(crate) fn restore_gateway_spawn_config(&mut self, config: GatewaySpawnConfig) {
        self.gateway_spawn_config = Some(config);
    }

    /// Whether the gateway session event bridge has been started.
    pub fn gateway_event_bridge_started(&self) -> bool {
        self.gateway_event_bridge_started
    }

    /// Mark whether the gateway event bridge has been started.
    pub fn set_gateway_event_bridge_started(&mut self, started: bool) {
        self.gateway_event_bridge_started = started;
    }

    /// Get the attached gateway terminal ID for manual keyboard input.
    pub fn gateway_terminal_id(&self) -> Option<&str> {
        self.gateway_terminal_id.as_deref()
    }

    /// Set the attached gateway terminal ID.
    pub fn set_gateway_terminal_id(&mut self, id: String) {
        self.gateway_terminal_id = Some(id);
    }

    /// Whether a Teams inbox nudge is pending for this pane.
    pub fn pending_inbox_nudge(&self) -> bool {
        self.pending_inbox_nudge
    }

    /// When the current pending Teams inbox nudge was first queued.
    pub fn pending_inbox_nudge_since(&self) -> Option<Instant> {
        self.pending_inbox_nudge_since
    }

    /// Set whether a Teams inbox nudge is pending.
    pub fn set_pending_inbox_nudge(&mut self, pending: bool) {
        self.pending_inbox_nudge = pending;
        if pending {
            self.pending_inbox_nudge_since
                .get_or_insert_with(Instant::now);
        } else {
            self.pending_inbox_nudge_since = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_pending_inbox_nudge_since(&mut self, since: Option<Instant>) {
        self.pending_inbox_nudge_since = since;
    }

    pub(crate) fn arm_claude_inbox_nudge_grace_period(&mut self) {
        if self.cli_type.capabilities().supports_teams {
            self.inbox_nudge_not_before = Some(Instant::now() + CLAUDE_TEAMS_STARTUP_SETTLE_DELAY);
        } else {
            self.inbox_nudge_not_before = None;
        }
    }

    pub fn inbox_nudge_not_before(&self) -> Option<Instant> {
        self.inbox_nudge_not_before
    }

    pub fn set_inbox_nudge_not_before(&mut self, at: Option<Instant>) {
        self.inbox_nudge_not_before = at;
    }

    /// Check if a manual input event should clear a pending inbox nudge.
    pub fn should_clear_pending_inbox_nudge_on_manual_input(&self, data: &[u8]) -> bool {
        self.pending_inbox_nudge
            && self.cli_type.capabilities().supports_teams
            && Self::is_manual_enter(data)
            && self.has_empty_claude_prompt_marker()
    }

    /// Append a visual notice that a message is queued in the Teams inbox.
    pub fn append_inbox_queue_notice(&mut self, from: &str) -> Result<()> {
        let notice = format!(
            "\x1b[2minbox: queued message from {from}; press Enter at an empty prompt to pick it up\x1b[0m\r\n"
        );
        self.append_output(notice.as_bytes())
    }

    /// Returns true if this pane is backed by an ACP gateway session
    /// rather than a PTY.
    pub fn is_gateway_backed(&self) -> bool {
        self.gateway_session_id.is_some() || self.gateway_spawn_config.is_some()
    }

    /// Get the activity buffer for gateway-backed panes, if allocated.
    pub fn activity_buffer(&self) -> Option<&ActivityBuffer> {
        self.activity_buffer.as_ref()
    }

    /// Get a mutable reference to the activity buffer, if allocated.
    pub fn activity_buffer_mut(&mut self) -> Option<&mut ActivityBuffer> {
        self.activity_buffer.as_mut()
    }

    /// Allocate the activity buffer on demand for gateway-backed panes.
    pub fn ensure_activity_buffer(&mut self) {
        if self.is_gateway_backed() && self.activity_buffer.is_none() {
            self.activity_buffer = Some(ActivityBuffer::default());
        }
    }

    /// Get the task context snapshot for this worker pane, if assigned.
    pub fn task_context(&self) -> Option<&TaskContextSnapshot> {
        self.task_context.as_ref()
    }

    /// Get a mutable reference to the task context snapshot.
    pub fn task_context_mut(&mut self) -> Option<&mut TaskContextSnapshot> {
        self.task_context.as_mut()
    }

    /// Set the task context snapshot for this pane.
    pub fn set_task_context(&mut self, context: TaskContextSnapshot) {
        self.task_context = Some(context);
    }

    /// Clear the task context (e.g., when a task completes).
    pub fn clear_task_context(&mut self) {
        self.task_context = None;
    }

    /// Get the review context snapshot for this reviewer pane, if active.
    pub fn review_context(&self) -> Option<&ReviewContextSnapshot> {
        self.review_context.as_ref()
    }

    /// Get the task id associated with this pane, preferring task context and
    /// then falling back to review ownership when the pane is reviewer-owned.
    pub fn assignment_task_id(&self) -> Option<String> {
        self.task_context()
            .map(|task| task.task_id.clone())
            .or_else(|| self.review_context().map(|review| review.task_id.clone()))
    }

    /// Get a mutable reference to the review context snapshot.
    pub fn review_context_mut(&mut self) -> Option<&mut ReviewContextSnapshot> {
        self.review_context.as_mut()
    }

    /// Set the review context snapshot for this pane.
    pub fn set_review_context(&mut self, context: ReviewContextSnapshot) {
        self.review_context = Some(context);
    }

    /// Clear the review context (e.g., when a review round completes).
    pub fn clear_review_context(&mut self) {
        self.review_context = None;
    }

    /// Get the timestamp of the last observed PTY output.
    pub fn last_output_at(&self) -> Instant {
        self.last_output_at
    }

    /// Set the last output timestamp.
    pub fn set_last_output_at(&mut self, at: Instant) {
        self.last_output_at = at;
    }

    /// Record output activity by updating the last output timestamp to now.
    pub fn record_output_activity(&mut self) {
        self.last_output_at = Instant::now();
    }

    /// Whether a tool is currently executing in this pane.
    pub fn is_tool_executing(&self) -> bool {
        self.is_tool_executing
    }

    /// Whether this pane can accept manual keyboard input.
    pub fn accepts_manual_input(&self) -> bool {
        self.panesmith_managed
            || matches!(self.backend, PaneBackend::Pty(_))
            || self.is_gateway_backed()
    }

    /// Check if this pane has been idle (no output and no tool executing) for longer than `threshold`.
    pub fn is_idle(&self, now: Instant, threshold: Duration) -> bool {
        now.saturating_duration_since(self.last_output_at) > threshold && !self.is_tool_executing()
    }

    /// Whether this pane is quiet enough to receive a Teams inbox nudge.
    pub fn is_ready_for_inbox_nudge(&self, now: Instant, quiet_threshold: Duration) -> bool {
        if !matches!(self.backend, PaneBackend::Pty(_)) {
            return false;
        }

        if self.focused {
            return false;
        }

        if now.saturating_duration_since(self.last_output_at) <= quiet_threshold {
            return false;
        }

        if self
            .inbox_nudge_not_before
            .is_some_and(|not_before| now < not_before)
        {
            return false;
        }

        if self.cli_type.capabilities().supports_teams {
            self.has_empty_claude_prompt_marker()
        } else {
            true
        }
    }

    pub(crate) fn has_pending_ink_submit(&self) -> bool {
        self.pending_ink_submit.lock().is_some()
    }

    pub(crate) fn has_nonempty_ink_prompt_marker(&self) -> bool {
        if !self.cli_type.capabilities().uses_ink_prompt {
            return false;
        }

        for row in (0..self.rows).rev() {
            let Ok(text) = self.dump_row(row) else {
                continue;
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }

            let lower = trimmed.to_ascii_lowercase();
            if lower.contains("tab to queue message") || lower.ends_with("context left") {
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix('>') {
                return !rest.trim().is_empty();
            }

            break;
        }

        false
    }

    pub(crate) fn has_active_ink_turn_marker(&self) -> bool {
        if !self.cli_type.capabilities().uses_ink_prompt {
            return false;
        }

        let start = self.rows.saturating_sub(8);
        for row in start..self.rows {
            let Ok(text) = self.dump_row(row) else {
                continue;
            };
            let lower = text.trim().to_ascii_lowercase();
            if lower.contains("esc to interrupt") || lower.contains("working (") {
                return true;
            }
        }

        false
    }

    pub(crate) fn is_ready_for_ink_prompt_injection(
        &self,
        now: Instant,
        quiet_threshold: Duration,
    ) -> bool {
        if !matches!(self.backend, PaneBackend::Pty(_)) {
            return false;
        }

        if !self.cli_type.capabilities().uses_ink_prompt {
            return true;
        }

        if now.saturating_duration_since(self.last_output_at) <= quiet_threshold {
            return false;
        }

        !self.has_pending_ink_submit()
            && !self.has_active_ink_turn_marker()
            && !self.has_nonempty_ink_prompt_marker()
    }

    /// Queue a buffered message for later injection.
    #[allow(dead_code)] // WIP: used by upcoming message injection logic
    pub(crate) fn queue_message(&mut self, message: BufferedMessage) {
        self.pending_messages.push_back(message);
    }

    /// Number of queued messages waiting for injection.
    pub fn pending_message_count(&self) -> usize {
        self.pending_messages.len()
    }

    /// Check if a specific prompt ID is in the pending message queue.
    pub fn has_pending_prompt(&self, prompt_id: i64) -> bool {
        self.pending_messages
            .iter()
            .any(|message| message.prompt_id == prompt_id)
    }

    /// Peek at the next queued message without removing it.
    #[allow(dead_code)] // WIP: used by upcoming message injection logic
    pub(crate) fn pending_message_front(&self) -> Option<&BufferedMessage> {
        self.pending_messages.front()
    }

    /// Remove and return the next queued message.
    #[allow(dead_code)] // WIP: used by upcoming message injection logic
    pub(crate) fn pop_pending_message(&mut self) -> Option<BufferedMessage> {
        self.pending_messages.pop_front()
    }

    /// Get the current cursor position as `(col, row)`.
    pub fn cursor_position(&self) -> (u16, u16) {
        self.terminal.cursor_position()
    }

    /// Resize the terminal and PTY to the given dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        if debug_log_enabled() {
            tracing::debug!(
                "Pane {}: resize from {}x{} to {}x{}",
                self.id,
                self.rows,
                self.cols,
                rows,
                cols
            );
        }
        self.terminal.resize(rows, cols).map_err(|e| {
            tracing::warn!("Pane {}: terminal.resize failed: {}", self.id, e);
            Error::terminal(e.to_string())
        })?;
        self.rows = rows;
        self.cols = cols;
        // Resize invalidates any cached viewport row data on the render
        // side, both because dimensions changed and because the parser
        // reflowed content; force a re-fetch on the next frame.
        self.bump_render_generation();
        match &self.backend {
            PaneBackend::Pty(pty) => pty.resize(rows, cols)?,
            PaneBackend::None => {}
        }
        Ok(())
    }

    /// Feed raw bytes into the terminal emulator (parses escape sequences).
    pub fn feed(&mut self, data: &[u8]) -> Result<()> {
        let result = self
            .terminal
            .feed(data)
            .map_err(|e| Error::terminal(e.to_string()));
        if !data.is_empty() {
            self.bump_render_generation();
        }
        result
    }

    pub(crate) fn feed_pty_output(&mut self, ghostty_data: &[u8]) -> Result<()> {
        let result = self
            .terminal
            .feed(ghostty_data)
            .map_err(|e| Error::terminal(e.to_string()));
        if !ghostty_data.is_empty() {
            self.bump_render_generation();
        }
        result
    }

    /// Append synthetic output to this pane with newline normalization and filtering.
    pub fn append_output(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.record_output_activity();
        trace_supervisor_bytes(&self.id, self.kind(), "append_output.input", data);
        let normalized = self.normalize_synthetic_newlines(data);
        let maybe_prefixed = self.prefix_status_line_if_mid_row(&normalized);
        let feed_data = self.prepare_output_for_feed(maybe_prefixed.as_ref());
        trace_supervisor_bytes(&self.id, self.kind(), "append_output.feed", &feed_data);
        self.feed(&feed_data)
    }

    fn prefix_status_line_if_mid_row<'a>(&self, data: &'a [u8]) -> Cow<'a, [u8]> {
        if data.is_empty() || !Self::looks_like_synthetic_status_line(data) {
            return Cow::Borrowed(data);
        }

        let (cursor_col, _) = self.cursor_position();
        if cursor_col <= 1 {
            return Cow::Borrowed(data);
        }

        let mut prefixed = Vec::with_capacity(data.len() + 2);
        prefixed.extend_from_slice(b"\r\n");
        prefixed.extend_from_slice(data);
        Cow::Owned(prefixed)
    }

    fn looks_like_synthetic_status_line(data: &[u8]) -> bool {
        let text = String::from_utf8_lossy(data);
        let text = text.trim_start_matches(['\r', '\n']);
        let stripped = text
            .strip_prefix("\x1b[")
            .and_then(|rest| rest.split_once('m'));
        let Some((_, body)) = stripped else {
            return false;
        };
        let body = body.strip_suffix("\x1b[0m\r\n").unwrap_or(body);
        let body = body.strip_suffix("\x1b[0m").unwrap_or(body).trim();

        body.starts_with("tool: ")
            || body.starts_with("mcp: ")
            || body.starts_with("inbox: ")
            || body.starts_with("permission request: ")
            || body == "response failed"
            || body.ends_with(" started")
            || body.ends_with(" complete")
            || body.ends_with(" failed")
    }

    pub(crate) fn prepare_output_for_feed(&mut self, data: &[u8]) -> Vec<u8> {
        let filtered = self.filter_supervisor_idle_chatter(data);
        match Self::strip_literal_cursor_reports(&filtered) {
            Cow::Borrowed(_) => filtered,
            Cow::Owned(stripped) => stripped,
        }
    }

    pub(crate) fn has_empty_claude_prompt_marker(&self) -> bool {
        matches!(self.claude_prompt_state(), ClaudePromptState::Empty)
    }

    /// Inspect the trailing rows of the terminal viewport to classify the
    /// Claude Code input box. Returns `None` for non-Claude CLIs or when no
    /// Claude prompt markers are visible yet. Used by the supervisor inbox
    /// recovery path to decide between Ctrl-C clear, defer, and inject.
    pub(crate) fn claude_prompt_state(&self) -> ClaudePromptState {
        for row in (0..self.rows).rev() {
            let Ok(text) = self.dump_row(row) else {
                continue;
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix('\u{276F}') {
                return if rest.trim().is_empty() {
                    ClaudePromptState::Empty
                } else {
                    ClaudePromptState::Draft
                };
            }

            if trimmed.contains("\u{2500}\u{2500}\u{2500}\u{2500}") && trimmed.contains('@') {
                return ClaudePromptState::Visible;
            }
        }

        ClaudePromptState::None
    }

    pub(crate) fn is_manual_enter(data: &[u8]) -> bool {
        matches!(data, b"\r" | b"\n" | b"\r\n")
    }

    fn filter_supervisor_idle_chatter(&mut self, data: &[u8]) -> Vec<u8> {
        if self.kind != PaneKind::Supervisor {
            return data.to_vec();
        }

        trace_supervisor_bytes(&self.id, &self.kind, "filter.input", data);

        if debug_log_enabled() {
            tracing::debug!(
                pane = %self.id,
                pending_len = self.supervisor_pending_structured_output.len(),
                raw = %debug_escape_bytes(data),
                "supervisor filter input"
            );
        }

        // Combine any carried-over bytes with the new data.
        let mut combined =
            Vec::with_capacity(self.supervisor_pending_structured_output.len() + data.len());
        if !self.supervisor_pending_structured_output.is_empty() {
            combined.extend_from_slice(&self.supervisor_pending_structured_output);
            self.supervisor_pending_structured_output.clear();
        }
        combined.extend_from_slice(data);

        // Find the last valid UTF-8 boundary so we don't corrupt multi-byte
        // sequences that straddle chunk boundaries.
        let safe_len = find_utf8_safe_boundary(&combined);
        if safe_len < combined.len() {
            // Carry the trailing incomplete bytes for the next chunk.
            self.supervisor_pending_structured_output
                .extend_from_slice(&combined[safe_len..]);
            combined.truncate(safe_len);
        }

        // TUI-mode bypass: if the combined chunk contains any ANSI escape byte
        // (0x1b), Claude (or any other agent CLI) is rendering a full-screen
        // TUI rather than emitting the plain-text JSON blocks this filter was
        // built to suppress. Buffering TUI frames here desyncs ghostty's
        // synchronized-output hints (\x1b[?2026h / \x1b[?2026l) and corrupts
        // rendering (see supervisor trace: every TUI chunk contains ESC; the
        // old path buffered whole frames whenever the visible text happened to
        // contain `brehon - task (MCP)(action:` in Claude's status area).
        //
        // Pass the combined buffer through unchanged; any previously buffered
        // plain-text partial block has already been prepended via `combined`,
        // so no state is lost.
        if is_tui_frame(&combined) {
            trace_supervisor_bytes(&self.id, &self.kind, "filter.output", &combined);
            return combined;
        }

        let text = String::from_utf8_lossy(&combined);
        let mut filtered = String::with_capacity(text.len());
        let mut pending_structured_block = String::new();
        let mut collecting_structured_block = false;

        for segment in text.split_inclusive('\n') {
            let line = segment.trim_end_matches(['\r', '\n']);
            let normalized_raw = line
                .trim_start()
                .trim_start_matches(['\u{2022}', '-', '*', ' '])
                .trim_start();
            let normalized_stripped = strip_terminal_control_sequences(normalized_raw);
            let normalized = normalized_stripped.as_str();

            if debug_log_enabled()
                && (collecting_structured_block
                    || normalized.contains("brehon")
                    || normalized.contains("mcp__brehon__agent")
                    || normalized.contains("\"count\":")
                    || normalized.contains("\"tasks\":")
                    || normalized.contains("\"task_type\":")
                    || normalized.contains("\"session_id\":")
                    || normalized.contains("Supervisor online"))
            {
                tracing::debug!(
                    pane = %self.id,
                    collecting = collecting_structured_block,
                    segment = %debug_escape_text(segment),
                    normalized = %debug_escape_text(normalized),
                    "supervisor filter segment"
                );
            }

            if collecting_structured_block {
                pending_structured_block.push_str(segment);

                if line.is_empty() {
                    if debug_log_enabled() {
                        tracing::debug!(
                            pane = %self.id,
                            suppress = is_supervisor_suppressible_structured_block(&pending_structured_block),
                            block = %debug_escape_text(&pending_structured_block),
                            "supervisor filter completed block"
                        );
                    }
                    if !is_supervisor_suppressible_structured_block(&pending_structured_block) {
                        filtered.push_str(&pending_structured_block);
                    }
                    pending_structured_block.clear();
                    collecting_structured_block = false;
                }
                continue;
            }

            if is_supervisor_structured_block_start(normalized) {
                if debug_log_enabled() {
                    tracing::debug!(
                        pane = %self.id,
                        normalized = %debug_escape_text(normalized),
                        "supervisor filter detected structured block start"
                    );
                }
                pending_structured_block.push_str(segment);
                collecting_structured_block = true;
                continue;
            }

            if is_supervisor_idle_filler_line(normalized) {
                continue;
            }

            filtered.push_str(segment);
        }

        if collecting_structured_block {
            if is_supervisor_structured_block_complete(&pending_structured_block) {
                if debug_log_enabled() {
                    tracing::debug!(
                        pane = %self.id,
                        suppress = is_supervisor_suppressible_structured_block(&pending_structured_block),
                        block = %debug_escape_text(&pending_structured_block),
                        "supervisor filter completed trailing block"
                    );
                }
                if !is_supervisor_suppressible_structured_block(&pending_structured_block) {
                    filtered.push_str(&pending_structured_block);
                }
            } else {
                if debug_log_enabled() {
                    tracing::debug!(
                        pane = %self.id,
                        block = %debug_escape_text(&pending_structured_block),
                        "supervisor filter buffering partial block"
                    );
                }
                // Store the incomplete block as bytes for the next chunk.
                self.supervisor_pending_structured_output
                    .extend_from_slice(pending_structured_block.as_bytes());
            }
        }

        if debug_log_enabled() {
            tracing::debug!(
                pane = %self.id,
                filtered = %debug_escape_bytes(filtered.as_bytes()),
                pending_len = self.supervisor_pending_structured_output.len(),
                "supervisor filter output"
            );
        }

        trace_supervisor_bytes(&self.id, &self.kind, "filter.output", filtered.as_bytes());

        filtered.into_bytes()
    }

    pub(crate) fn normalize_synthetic_newlines(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 8);
        let mut prev_was_cr = self.synthetic_prev_was_cr;

        for &byte in data {
            match byte {
                b'\n' => {
                    if !prev_was_cr {
                        out.push(b'\r');
                    }
                    out.push(b'\n');
                    prev_was_cr = false;
                }
                b'\r' => {
                    out.push(b'\r');
                    prev_was_cr = true;
                }
                _ => {
                    out.push(byte);
                    prev_was_cr = false;
                }
            }
        }

        self.synthetic_prev_was_cr = prev_was_cr;
        out
    }

    /// Strip literal cursor-position report echoes such as `^[[1;1R`.
    ///
    /// Some agent CLIs emit this as plain text when probing terminal support,
    /// which creates visual noise in pane output.
    pub(super) fn strip_literal_cursor_reports(data: &[u8]) -> Cow<'_, [u8]> {
        let mut out: Option<Vec<u8>> = None;
        let mut i = 0usize;
        let mut last_emit = 0usize;

        while i < data.len() {
            if let Some(len) = Self::literal_cursor_report_len(&data[i..]) {
                let out_buf = out.get_or_insert_with(|| Vec::with_capacity(data.len()));
                out_buf.extend_from_slice(&data[last_emit..i]);
                i += len;
                last_emit = i;
                continue;
            }
            i += 1;
        }

        if let Some(mut out_buf) = out {
            out_buf.extend_from_slice(&data[last_emit..]);
            Cow::Owned(out_buf)
        } else {
            Cow::Borrowed(data)
        }
    }

    fn literal_cursor_report_len(data: &[u8]) -> Option<usize> {
        // Matches: ^[[<row>;<col>R
        if data.len() < 7 || data[0] != b'^' || data[1] != b'[' || data[2] != b'[' {
            return None;
        }

        let mut idx = 3;
        let row_start = idx;
        while idx < data.len() && data[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == row_start || idx >= data.len() || data[idx] != b';' {
            return None;
        }

        idx += 1;
        let col_start = idx;
        while idx < data.len() && data[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == col_start || idx >= data.len() || data[idx] != b'R' {
            return None;
        }

        Some(idx + 1)
    }

    /// Dump the entire viewport as a newline-joined string.
    pub fn dump_viewport(&self) -> Result<String> {
        Ok(self
            .viewport_rows_for_display()?
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Dump the text content of a single viewport row.
    pub fn dump_row(&self, row: u16) -> Result<String> {
        self.terminal
            .dump_viewport_row(row)
            .map_err(|e| Error::terminal(e.to_string()))
    }

    /// Get per-cell styles for a single viewport row.
    pub fn row_styles(&self, row: u16) -> Result<Vec<CellStyle>> {
        self.terminal
            .row_cell_styles(row)
            .map_err(|e| Error::terminal(e.to_string()))
    }

    /// Render a single viewport row as a styled ratatui `Line`.
    pub fn row_as_line(&self, row: u16) -> Result<Line<'static>> {
        let text = self.dump_row(row)?;
        let styles = self.row_styles(row)?;
        Ok(build_styled_line(&text, &styles))
    }

    /// Efficiently render all viewport rows into styled lines.
    ///
    /// Dumps text for all rows in a single bulk pass (including supervisor
    /// cleaning), then pairs each row with its cell styles.  This is O(N)
    /// in FFI calls instead of the O(N^2) that per-row `row_as_line` would
    /// incur via `dump_row -> viewport_rows_for_display`.
    pub fn viewport_as_lines(&self) -> Result<Vec<Line<'static>>> {
        let rows = self.viewport_rows_for_display()?;
        let mut lines = Vec::with_capacity(rows.len());
        for row in &rows {
            let styles = self.row_styles(row.source_row)?;
            lines.push(build_styled_line(&row.text, &styles));
        }
        Ok(lines)
    }

    /// Get the cursor position in the filtered display viewport.
    pub fn display_cursor_position(&self) -> Result<Option<(u16, u16)>> {
        let rows = self.viewport_rows_for_display()?;
        let (cursor_col, cursor_row) = self.cursor_position();
        Ok(display_cursor_position_for_rows(
            &rows, cursor_col, cursor_row,
        ))
    }

    pub(crate) fn viewport_rows_for_display(&self) -> Result<Vec<DisplayViewportRow>> {
        let mut rows = Vec::with_capacity(self.rows as usize);
        for row in 0..self.rows {
            let text = self
                .terminal
                .dump_viewport_row(row)
                .map_err(|e| Error::terminal(e.to_string()))?;
            rows.push(DisplayViewportRow {
                source_row: row,
                text,
            });
        }
        Ok(rows)
    }

    /// Scroll offset for the renderer currently used by the TUI.
    pub fn display_scroll_offset(&self) -> u32 {
        self.scroll_offset()
    }

    /// Scroll the viewport by `delta` lines (positive = down, negative = up).
    pub fn scroll(&mut self, delta: i32) -> Result<()> {
        let info_before = self.terminal.scrollback_info();
        if debug_log_enabled() {
            tracing::debug!(
                "Pane {}: scroll delta={}, before: offset={}, total={}",
                self.id,
                delta,
                info_before.viewport_offset,
                info_before.total_scrollback
            );
        }
        let result = self
            .terminal
            .scroll(delta)
            .map_err(|e| Error::terminal(e.to_string()));
        let info_after = self.terminal.scrollback_info();
        if debug_log_enabled() {
            tracing::debug!(
                "Pane {}: scroll complete, after: offset={}, total={}",
                self.id,
                info_after.viewport_offset,
                info_after.total_scrollback
            );
        }
        if info_after.viewport_offset != info_before.viewport_offset {
            self.bump_render_generation();
        }
        result
    }

    /// Scroll to the top of the scrollback buffer.
    pub fn scroll_to_top(&mut self) -> Result<()> {
        let result = self
            .terminal
            .scroll_to_top()
            .map_err(|e| Error::terminal(e.to_string()));
        self.bump_render_generation();
        result
    }

    /// Scroll to the bottom (most recent content).
    pub fn scroll_to_bottom(&mut self) -> Result<()> {
        let result = self
            .terminal
            .scroll_to_bottom()
            .map_err(|e| Error::terminal(e.to_string()));
        self.bump_render_generation();
        result
    }

    /// Mark this pane's process as exited with an optional exit code.
    pub fn mark_exited(&mut self, exit_code: Option<i32>) {
        self.exited = true;
        self.exit_code = exit_code;
        self.set_tool_executing(false);
        self.set_pending_inbox_nudge(false);
        self.pending_messages.clear();
        self.prompt_queue = Default::default();
        if !matches!(self.pane_state.as_ref(), Some(PaneState::Dead { .. })) {
            self.pane_state = Some(PaneState::Dead {
                reason: DeathReason::SessionDropped,
                at: Instant::now(),
            });
        }
    }
}

fn refresh_brehon_session_id(config: &mut PtyConfig) {
    let new_session_id = uuid::Uuid::new_v4().to_string();
    let mut found_env = false;
    for (key, value) in &mut config.env {
        if key == "BREHON_SESSION_ID" {
            *value = new_session_id.clone();
            found_env = true;
        }
    }
    if !found_env {
        config
            .env
            .push(("BREHON_SESSION_ID".to_string(), new_session_id.clone()));
    }

    let mut idx = 0;
    while idx + 1 < config.args.len() {
        if config.args[idx] == "--session-id" {
            config.args[idx + 1] = new_session_id.clone();
            break;
        }
        idx += 1;
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn is_supervisor_idle_filler_line(line: &str) -> bool {
    let sanitized = strip_terminal_control_sequences(line);
    let line = sanitized.trim();
    if line.is_empty() {
        return false;
    }

    let lower = line.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "waiting." | "still waiting." | "standing by." | "idle."
    ) || lower.starts_with("idle. waiting ")
        || lower.starts_with("waiting for ")
}

fn display_cursor_position_for_rows(
    rows: &[DisplayViewportRow],
    cursor_col: u16,
    cursor_row: u16,
) -> Option<(u16, u16)> {
    let source_row = cursor_row.saturating_sub(1);
    let display_row = rows.iter().position(|row| row.source_row == source_row)?;
    Some((cursor_col, display_row as u16 + 1))
}

fn is_supervisor_structured_block_start(line: &str) -> bool {
    let sanitized = strip_terminal_control_sequences(line);
    let line = sanitized.trim();
    line.contains("brehon - task (MCP)(action:")
        || line.contains("mcp__brehon__agent action=whoami")
}

fn is_supervisor_structured_block_complete(block: &str) -> bool {
    strip_terminal_control_sequences(block)
        .trim_end()
        .ends_with('}')
}

fn is_supervisor_suppressible_structured_block(block: &str) -> bool {
    is_supervisor_empty_task_ready_block(block)
        || is_supervisor_empty_epic_list_block(block)
        || is_supervisor_whoami_block(block)
        || is_supervisor_agent_startup_block(block)
}

fn is_supervisor_empty_task_ready_block(block: &str) -> bool {
    let sanitized = strip_terminal_control_sequences(block);
    sanitized.contains("brehon - task (MCP)(action:")
        && sanitized.contains("\"ready\"")
        && sanitized.contains("\"count\": 0")
        && sanitized.contains("\"tasks\": []")
}

fn is_supervisor_empty_epic_list_block(block: &str) -> bool {
    let sanitized = strip_terminal_control_sequences(block);
    sanitized.contains("brehon - task (MCP)(action:")
        && sanitized.contains("\"list\"")
        && sanitized.contains("\"task_type\": \"epic\"")
        && sanitized.contains("\"count\": 0")
        && sanitized.contains("\"tasks\": []")
}

fn is_supervisor_whoami_block(block: &str) -> bool {
    let sanitized = strip_terminal_control_sequences(block);
    sanitized.contains("mcp__brehon__agent action=whoami")
        && sanitized.contains("\"agent_name\":")
        && sanitized.contains("\"role\":")
        && sanitized.contains("\"session_id\":")
}

fn is_supervisor_agent_startup_block(block: &str) -> bool {
    let sanitized = strip_terminal_control_sequences(block);
    sanitized.contains("brehon - agent (MCP)")
        || (sanitized.contains("session_start")
            && (sanitized.contains("\"status\": \"ok\"")
                || sanitized.contains("\"status\":\"ok\"")))
}

/// Build a styled `Line` from raw text and per-cell style data.
///
/// Groups consecutive characters that share the same style into a single
/// `Span`, producing the minimum number of spans needed.
pub(crate) fn build_styled_line(text: &str, styles: &[CellStyle]) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Line::from(Vec::<Span<'static>>::new());
    }

    // Map each char to its starting terminal column. ghostty_vt returns one
    // CellStyle per column, so wide characters (width 2) shift all subsequent
    // column indices.
    let mut col_map: Vec<usize> = Vec::with_capacity(chars.len());
    let mut col = 0usize;
    for &ch in &chars {
        col_map.push(col);
        col += ch.width().unwrap_or(0).max(1);
    }

    let mut spans = Vec::new();
    let mut run_start = 0usize; // char index where current run began

    for i in 1..=chars.len() {
        let at_end = i == chars.len();
        let style_changed = !at_end
            && col_map[i] < styles.len()
            && col_map[run_start] < styles.len()
            && !styles_equal(&styles[col_map[run_start]], &styles[col_map[i]]);

        if at_end || style_changed {
            let span_text: String = chars[run_start..i].iter().collect();
            let style = if col_map[run_start] < styles.len() {
                cell_style_to_ratatui(&styles[col_map[run_start]])
            } else {
                ratatui::style::Style::default()
            };
            spans.push(Span::styled(span_text, style));
            run_start = i;
        }
    }

    if spans.is_empty() && !text.is_empty() {
        spans.push(Span::raw(text.to_string()));
    }

    Line::from(spans)
}

pub(crate) fn strip_terminal_control_sequences(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }

            match bytes[i] {
                b'[' => {
                    i += 1;
                    while i < bytes.len() {
                        let byte = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                b']' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    i += 1;
                }
            }
            continue;
        }

        let ch = text[i..].chars().next().expect("valid utf-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn is_tui_frame(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i < data.len() {
        if data[i] != 0x1b {
            i += 1;
            continue;
        }
        i += 1;
        if i >= data.len() || data[i] != b'[' {
            continue;
        }
        i += 1;
        if i < data.len() && data[i] == b'?' {
            return true;
        }
        let mut end = i;
        while end < data.len()
            && (data[end].is_ascii_digit() || data[end] == b';' || data[end] == b'?')
        {
            end += 1;
        }
        if end < data.len() {
            let final_byte = data[end];
            match final_byte {
                b'H' | b'f' | b'A' | b'B' | b'C' | b'D' | b'J' | b'K' | b'S' | b'T' => return true,
                _ => {}
            }
            i = end + 1;
        }
    }
    false
}

/// Find the largest prefix of `data` that ends on a valid UTF-8 boundary.
///
/// If the last 1-3 bytes form an incomplete multi-byte leader, the returned
/// length excludes them so callers can carry them to the next chunk.
pub(crate) fn find_utf8_safe_boundary(data: &[u8]) -> usize {
    // Walk backward up to 3 bytes looking for an incomplete multi-byte start.
    let len = data.len();
    for i in 1..=3.min(len) {
        let b = data[len - i];
        if b < 0x80 {
            // ASCII — everything up to here is safe.
            return len;
        }
        // Check for a leading byte (0b11xxxxxx) that expects more continuation
        // bytes than are available after it.
        if b & 0xC0 == 0xC0 {
            // Determine expected sequence length from the leading byte.
            let expected = if b & 0xF8 == 0xF0 {
                4
            } else if b & 0xF0 == 0xE0 {
                3
            } else if b & 0xE0 == 0xC0 {
                2
            } else {
                // Invalid leading byte — treat as safe boundary.
                return len;
            };
            let available = i; // bytes from this leader to end of data
            if available < expected {
                // Incomplete sequence — split before the leader.
                return len - i;
            }
            // Complete sequence — all bytes are safe.
            return len;
        }
        // Continuation byte (0b10xxxxxx) — keep scanning backward for leader.
    }
    len
}

pub(crate) fn debug_escape_bytes(data: &[u8]) -> String {
    String::from_utf8_lossy(data).escape_debug().to_string()
}

pub(crate) fn debug_escape_text(text: &str) -> String {
    text.escape_debug().to_string()
}

fn supervisor_trace_path() -> Option<PathBuf> {
    std::env::var_os("BREHON_SUPERVISOR_TRACE_FILE").map(PathBuf::from)
}

pub(crate) fn trace_supervisor_bytes(pane_id: &str, kind: &PaneKind, stage: &str, data: &[u8]) {
    if *kind != PaneKind::Supervisor {
        return;
    }
    let Some(path) = supervisor_trace_path() else {
        return;
    };
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "[{stage}] pane={pane_id} len={} data={}",
            data.len(),
            debug_escape_bytes(data)
        );
    }
}
