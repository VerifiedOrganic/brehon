//! Activity buffer methods for Mux.
//!
//! Pane state, context, scroll, and terminal snapshot methods.

use crate::error::{Error, Result};
use crate::mux::MuxEvent;
use crate::pane::{DeathReason, Generation, PaneState, ReviewContextSnapshot, TaskContextSnapshot};
use brehon_protocol::ServerMessage;
use brehon_types::{PromptId, RuntimeEvent, RuntimeEventKind, RuntimePaneState, RuntimeSource};
use std::time::Instant;

fn runtime_source_is_terminal_host(source: &RuntimeSource) -> bool {
    matches!(
        source,
        RuntimeSource::Headless | RuntimeSource::Web | RuntimeSource::NativeGui
    )
}

impl crate::mux::Mux {
    /// Mirror a terminal-host runtime event into the local pane model.
    ///
    /// Host-owned agent panes are constructed as metadata-only panes in the mux,
    /// so observed host output must be fed back into the local terminal emulator
    /// for the embedded TUI tabs to remain useful. Generation fencing keeps stale
    /// output from a recycled host pane from regressing the current pane.
    pub fn apply_terminal_host_runtime_event(&mut self, event: &RuntimeEvent) -> Result<bool> {
        if !runtime_source_is_terminal_host(&event.meta.source) {
            return Ok(false);
        }

        let Some(pane) = self.panes.get_mut(&event.meta.pane_id) else {
            return Ok(false);
        };

        let event_generation = Generation(event.meta.generation);
        if event_generation < pane.current_generation() {
            return Ok(false);
        }
        if event_generation > pane.current_generation() {
            pane.current_generation = event_generation;
        }

        let now = Instant::now();
        match &event.kind {
            RuntimeEventKind::PaneSpawned(spawned) => {
                if let Some(title) = spawned.title.as_ref() {
                    pane.set_title(title.clone());
                }
                pane.exited = false;
                pane.exit_code = None;
                pane.set_pane_state(PaneState::Ready { since: now });
                Ok(true)
            }
            RuntimeEventKind::PaneOutput(output) => {
                if !output.bytes.is_empty() {
                    pane.append_output(&output.bytes)?;
                    Ok(true)
                } else if let Some(text) = output.text.as_deref() {
                    if text.is_empty() {
                        Ok(false)
                    } else {
                        pane.append_output(text.as_bytes())?;
                        Ok(true)
                    }
                } else {
                    Ok(false)
                }
            }
            RuntimeEventKind::PaneStateChanged(changed) => {
                match changed.current {
                    RuntimePaneState::Ready => pane.set_pane_ready(now),
                    RuntimePaneState::Busy => {
                        let prompt_id = event.meta.correlation_id.clone().unwrap_or_else(|| {
                            format!("terminal-host-{}", event.meta.timestamp_ms)
                        });
                        pane.set_pane_busy(PromptId::new(prompt_id), event_generation, now);
                    }
                    RuntimePaneState::Dead => {
                        pane.set_tool_executing(false);
                        pane.set_pending_inbox_nudge(false);
                        pane.set_pane_state(PaneState::Dead {
                            reason: DeathReason::SessionDropped,
                            at: now,
                        });
                    }
                    RuntimePaneState::Unknown => {}
                }
                Ok(true)
            }
            RuntimeEventKind::PaneExited(exited) => {
                pane.mark_exited(exited.exit_code);
                pane.set_pane_state(PaneState::Dead {
                    reason: DeathReason::SessionDropped,
                    at: now,
                });
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Set task context on a worker pane by pane ID.
    /// Emits a TaskContextChanged event.
    pub fn set_pane_task_context(&mut self, pane_id: &str, context: TaskContextSnapshot) {
        if let Some(pane) = self.panes.get_mut(pane_id) {
            let ctx_clone = context.clone();
            pane.set_task_context(context);
            let _ = self.event_tx.try_send(MuxEvent::TaskContextChanged {
                pane_id: pane_id.to_string(),
                context: Some(ctx_clone),
            });
        }
    }

    /// Clear task context on a worker pane by pane ID.
    /// Emits a TaskContextChanged event with context=None.
    pub fn clear_pane_task_context(&mut self, pane_id: &str) {
        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.clear_task_context();
            let _ = self.event_tx.try_send(MuxEvent::TaskContextChanged {
                pane_id: pane_id.to_string(),
                context: None,
            });
        }
    }

    /// Set task context on a worker pane by agent session ID.
    /// Emits a TaskContextChanged event.
    pub fn set_pane_task_context_by_session(
        &mut self,
        session_id: &str,
        context: TaskContextSnapshot,
    ) {
        for (pane_id, pane) in self.panes.iter_mut() {
            if pane.agent_session_id() == Some(session_id) {
                let ctx_clone = context.clone();
                pane.set_task_context(context);
                let _ = self.event_tx.try_send(MuxEvent::TaskContextChanged {
                    pane_id: pane_id.clone(),
                    context: Some(ctx_clone),
                });
                return;
            }
        }
    }

    /// Clear task context on a worker pane by agent session ID.
    /// Emits a TaskContextChanged event with context=None.
    pub fn clear_pane_task_context_by_session(&mut self, session_id: &str) {
        for (pane_id, pane) in self.panes.iter_mut() {
            if pane.agent_session_id() == Some(session_id) {
                pane.clear_task_context();
                let _ = self.event_tx.try_send(MuxEvent::TaskContextChanged {
                    pane_id: pane_id.clone(),
                    context: None,
                });
                return;
            }
        }
    }

    /// Set review context on a reviewer pane by pane ID.
    /// Emits a ReviewContextChanged event.
    pub fn set_pane_review_context(&mut self, pane_id: &str, context: ReviewContextSnapshot) {
        if let Some(pane) = self.panes.get_mut(pane_id) {
            let ctx_clone = context.clone();
            pane.set_review_context(context);
            let _ = self.event_tx.try_send(MuxEvent::ReviewContextChanged {
                pane_id: pane_id.to_string(),
                context: Some(ctx_clone),
            });
        }
    }

    /// Clear review context on a reviewer pane by pane ID.
    /// Emits a ReviewContextChanged event with context=None.
    pub fn clear_pane_review_context(&mut self, pane_id: &str) {
        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.clear_review_context();
            let _ = self.event_tx.try_send(MuxEvent::ReviewContextChanged {
                pane_id: pane_id.to_string(),
                context: None,
            });
        }
    }

    /// Set review context on a reviewer pane by agent session ID.
    /// Emits a ReviewContextChanged event.
    pub fn set_pane_review_context_by_session(
        &mut self,
        session_id: &str,
        context: ReviewContextSnapshot,
    ) {
        for (pane_id, pane) in self.panes.iter_mut() {
            if pane.agent_session_id() == Some(session_id) {
                let ctx_clone = context.clone();
                pane.set_review_context(context);
                let _ = self.event_tx.try_send(MuxEvent::ReviewContextChanged {
                    pane_id: pane_id.clone(),
                    context: Some(ctx_clone),
                });
                return;
            }
        }
    }

    /// Clear review context on a reviewer pane by agent session ID.
    /// Emits a ReviewContextChanged event with context=None.
    pub fn clear_pane_review_context_by_session(&mut self, session_id: &str) {
        for (pane_id, pane) in self.panes.iter_mut() {
            if pane.agent_session_id() == Some(session_id) {
                pane.clear_review_context();
                let _ = self.event_tx.try_send(MuxEvent::ReviewContextChanged {
                    pane_id: pane_id.clone(),
                    context: None,
                });
                return;
            }
        }
    }

    /// Get incremental terminal updates for all panes with changes.
    ///
    /// Returns a list of (pane_id, ServerMessage::PaneRowsUpdate) for each pane
    /// that has dirty rows since the last call. This is the terminal-style approach
    /// where the server renders terminals and sends pre-rendered cells to clients.
    ///
    /// Call this after `poll_batch()` to get rendered updates instead of raw bytes.
    pub fn get_incremental_updates(&mut self) -> Vec<ServerMessage> {
        let mut updates = Vec::new();

        for (id, pane) in self.panes.iter_mut() {
            match pane.get_incremental_update() {
                Ok(Some((rows, cursor, seq))) => {
                    updates.push(ServerMessage::PaneRowsUpdate {
                        pane_id: id.clone(),
                        rows,
                        cursor,
                        seq,
                    });
                }
                Ok(None) => {
                    // No updates for this pane
                }
                Err(e) => {
                    tracing::warn!("Pane {}: get_incremental_update failed: {}", id, e);
                }
            }
        }

        updates
    }

    /// Get full terminal snapshot for a specific pane.
    ///
    /// Used for initial sync when a client connects or when scrollback is requested.
    pub fn get_pane_snapshot(
        &self,
        pane_id: &str,
    ) -> Option<(brehon_protocol::TerminalSnapshot, u32, u32)> {
        self.panes.get(pane_id).and_then(|pane| {
            pane.get_full_snapshot().ok().map(|snapshot| {
                (
                    snapshot,
                    pane.display_scroll_offset(),
                    pane.scrollback_lines(),
                )
            })
        })
    }

    /// Append synthetic output to a pane without requiring a PTY backend.
    pub fn append_output(&mut self, pane_id: &str, data: &[u8]) -> Result<()> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.append_output(data)
    }

    /// Mark a pane as exited when its backing transport ends outside the PTY path.
    pub fn mark_pane_exited(&mut self, pane_id: &str, exit_code: Option<i32>) -> Result<()> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.mark_exited(exit_code);
        Ok(())
    }

    /// Scroll the focused pane by delta lines
    ///
    /// Positive delta scrolls down (towards newer content), negative scrolls up (towards older content).
    pub fn scroll_focused(&mut self, delta: i32) -> Result<()> {
        let pane = self
            .focused_mut()
            .ok_or_else(|| Error::pty("No focused pane"))?;
        pane.scroll(delta)
    }

    /// Scroll the focused pane to top of scrollback
    pub fn scroll_focused_to_top(&mut self) -> Result<()> {
        let pane = self
            .focused_mut()
            .ok_or_else(|| Error::pty("No focused pane"))?;
        pane.scroll_to_top()
    }

    /// Scroll the focused pane to bottom (most recent content)
    pub fn scroll_focused_to_bottom(&mut self) -> Result<()> {
        let pane = self
            .focused_mut()
            .ok_or_else(|| Error::pty("No focused pane"))?;
        pane.scroll_to_bottom()
    }

    /// Scroll a specific pane by delta lines
    pub fn scroll_pane(&mut self, pane_id: &str, delta: i32) -> Result<()> {
        let pane = self
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.scroll(delta)
    }

    /// Scroll a specific pane and return snapshot with cache rows for smooth scrolling.
    ///
    /// This is the main entry point for handling Scroll messages with cache_window.
    /// Returns (snapshot, cache_rows, cache_start_row, scroll_offset, scrollback_lines).
    #[allow(clippy::type_complexity)]
    pub fn scroll_pane_with_cache(
        &mut self,
        pane_id: &str,
        delta: i32,
        cache_window: u32,
    ) -> Result<(
        brehon_protocol::TerminalSnapshot,
        Vec<brehon_protocol::CacheRow>,
        Option<u32>,
        u32,
        u32,
    )> {
        let pane = self
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;

        // Apply scroll
        pane.scroll(delta)?;

        // Get snapshot with cache rows
        let (snapshot, cache_rows, cache_start) = pane.create_snapshot_with_cache(cache_window)?;
        let scroll_offset = pane.display_scroll_offset();
        let scrollback_lines = pane.scrollback_lines();

        Ok((
            snapshot,
            cache_rows,
            cache_start,
            scroll_offset,
            scrollback_lines,
        ))
    }

    /// Scroll a specific pane and return RowData snapshot with cache rows.
    ///
    /// Returns (snapshot_rows, cache_rows, cache_start_row, scroll_offset, scrollback_lines).
    #[allow(clippy::type_complexity)]
    pub fn scroll_pane_with_cache_rows(
        &mut self,
        pane_id: &str,
        delta: i32,
        cache_window: u32,
    ) -> Result<(
        Vec<brehon_protocol::RowData>,
        Vec<brehon_protocol::CacheRow>,
        Option<u32>,
        u32,
        u32,
    )> {
        let pane = self
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;

        // Apply scroll
        pane.scroll(delta)?;

        // Get snapshot rows with cache
        let (snapshot_rows, cache_rows, cache_start) =
            pane.create_snapshot_rows_with_cache(cache_window)?;
        let scroll_offset = pane.display_scroll_offset();
        let scrollback_lines = pane.scrollback_lines();

        Ok((
            snapshot_rows,
            cache_rows,
            cache_start,
            scroll_offset,
            scrollback_lines,
        ))
    }

    /// Get the current scroll offset for a pane (lines from bottom).
    pub fn scroll_offset(&self, pane_id: &str) -> Option<u32> {
        self.get(pane_id).map(|p| p.display_scroll_offset())
    }

    /// Scroll a specific pane to bottom
    pub fn scroll_pane_to_bottom(&mut self, pane_id: &str) -> Result<()> {
        let pane = self
            .get_mut(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        pane.scroll_to_bottom()
    }
}
