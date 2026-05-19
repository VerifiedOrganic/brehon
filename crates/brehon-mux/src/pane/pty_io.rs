//! PTY I/O: poll, drain, write, prompt injection, recording, and cleanup.

use crate::error::{Error, Result};
use crate::harness::AgentAdapter;
use crate::pane::spawn::{
    PRE_SUBMIT_INTER_INTERRUPT_DELAY, PRE_SUBMIT_SETTLE_DELAY, uses_delayed_submit_injection,
    uses_ink_echo_injection, uses_pre_submit_interrupt_reset,
};
use crate::pane::terminal::trace_supervisor_bytes;
use crate::pane::types::{Pane, PaneBackend};
use crate::pty::{PtyEvent, format_cursor_position_report};
use brehon_recording::WriterConfig;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

/// Maximum coalesced bytes a single `Pane::drain_output` call will pull
/// from the pane's mpsc channel before yielding back to the TUI event
/// loop. Sized at ~3 KiB/row × 80 rows ≈ one screenful with generous
/// headroom — comfortably larger than any single Ink frame but small
/// enough that one chatty pane can't monopolise a tick. Leftover bytes
/// stay queued in the 1024-buffered mpsc channel (see
/// `crates/brehon-pty/src/pty/core.rs`) until the next tick drains them.
///
/// See § F8b in `tmp/tick-latency/GOAL_PROMPT.md`.
const DRAIN_OUTPUT_MAX_BYTES_PER_CALL: usize = 256 * 1024;

/// Maximum number of `PtyEvent`s drained per `Pane::drain_output` call.
/// Bounds wall-time even when each event is small (e.g. a burst of CPR
/// queries or empty outputs from a sync-filtered Ink frame) — without
/// this cap, the byte-budget alone could be defeated by thousands of
/// tiny events that don't accumulate bytes but still cost FFI feeds.
const DRAIN_OUTPUT_MAX_EVENTS_PER_CALL: usize = 64;

/// Result of one budgeted drain pass. Returned from
/// `drain_events_with_budget` so `Pane::drain_output` can apply
/// `&mut self` side effects (activity timestamps, exit marking) without
/// holding the helper closure's borrow.
struct DrainBudgetResult {
    coalesced: Vec<u8>,
    pending_cpr: bool,
    /// Number of `PtyEvent::Output` items drained — used to replay
    /// `record_output_activity` once per chunk so the activity-window
    /// accounting matches the pre-budget code path.
    output_event_count: usize,
    /// `Exited` / `Error` events forwarded to the caller verbatim.
    other_events: Vec<PtyEvent>,
}

/// Drain events from `recv` until exhausted OR a budget cap is hit,
/// whichever comes first. Pure with respect to `&mut self` on `Pane`
/// (the closure owns the recv side and the caller owns the side
/// effects). Factored out specifically so the budget logic is
/// unit-testable without standing up a real PTY backend.
///
/// Budget semantics: the byte cap is checked *after* extending
/// `coalesced` with the just-arrived chunk, so the final chunk may
/// overshoot the cap by its own size. The event cap is exact.
fn drain_events_with_budget<F>(
    mut recv: F,
    max_bytes: usize,
    max_events: usize,
) -> DrainBudgetResult
where
    F: FnMut() -> Option<PtyEvent>,
{
    let mut coalesced: Vec<u8> = Vec::with_capacity(65536);
    let mut pending_cpr = false;
    let mut output_event_count = 0usize;
    let mut other_events: Vec<PtyEvent> = Vec::new();
    let mut events_drained = 0usize;

    while let Some(event) = recv() {
        match event {
            PtyEvent::Output(data) => {
                coalesced.extend_from_slice(&data);
                output_event_count += 1;
            }
            PtyEvent::CursorPositionRequested => {
                pending_cpr = true;
            }
            PtyEvent::Exited(_) | PtyEvent::Error(_) => {
                other_events.push(event);
            }
        }
        events_drained += 1;
        if coalesced.len() >= max_bytes || events_drained >= max_events {
            break;
        }
    }

    DrainBudgetResult {
        coalesced,
        pending_cpr,
        output_event_count,
        other_events,
    }
}

/// A cloneable handle for performing PTY prompt injection from spawned tasks.
///
/// This extracts the minimal state needed for `inject_prompt` so that the TUI
/// can fire-and-forget prompt delivery without blocking on `&mut Pane`.
#[derive(Clone)]
pub struct PaneInjector {
    pub pane_id: String,
    pub cli_type: AgentAdapter,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub ink_generation: Arc<AtomicU64>,
    pub pending_ink: Arc<std::sync::Mutex<Option<(String, Instant, u64)>>>,
}

impl PaneInjector {
    /// Inject a prompt into the pane using CLI-specific key sequences.
    pub async fn inject_prompt(&self, prompt: &str) -> Result<()> {
        if uses_pre_submit_interrupt_reset(&self.cli_type) {
            self.ink_generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            tracing::debug!(
                cli = %self.cli_type,
                "pre-submit interrupt: sending double-tap Ctrl-C reset"
            );
            {
                let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
                w.write_all(&[0x03])?;
                w.flush()?;
            }
            tokio::time::sleep(PRE_SUBMIT_INTER_INTERRUPT_DELAY).await;
            {
                let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
                w.write_all(&[0x03])?;
                w.flush()?;
            }
            tokio::time::sleep(PRE_SUBMIT_SETTLE_DELAY).await;
        }
        if uses_ink_echo_injection(&self.cli_type) {
            let text = prompt.trim();
            {
                let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
                w.write_all(text.as_bytes())?;
                w.flush()?;
            }
            let needle: String = text
                .chars()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let generation = self
                .ink_generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let deadline = Instant::now() + Duration::from_secs(3);
            if let Ok(mut pending) = self.pending_ink.lock() {
                *pending = Some((needle, deadline, generation));
            }

            let writer = self.writer.clone();
            let pending_state = Arc::clone(&self.pending_ink);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let still_ours = pending_state
                    .lock()
                    .map(|p| {
                        p.as_ref()
                            .map(|(_, _, g)| *g == generation)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if still_ours {
                    tracing::warn!(
                        generation,
                        "Ink echo fallback: echo detection did not fire, \
                             sending Enter via guaranteed timer"
                    );
                    let mut w = writer.lock().expect("PTY writer mutex poisoned");
                    let _ = w.write_all(b"\r");
                    let _ = w.flush();
                    if let Ok(mut p) = pending_state.lock() {
                        *p = None;
                    }
                }
            });
        } else if uses_delayed_submit_injection(&self.cli_type) {
            let payload = format!("\x15{}", prompt.trim());
            {
                let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
                w.write_all(payload.as_bytes())?;
                w.flush()?;
            }

            let generation = self
                .ink_generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let writer = self.writer.clone();
            let submit_generation = Arc::clone(&self.ink_generation);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(75)).await;
                if submit_generation.load(std::sync::atomic::Ordering::Relaxed) == generation {
                    let mut w = writer.lock().expect("PTY writer mutex poisoned");
                    let _ = w.write_all(b"\r");
                    let _ = w.flush();
                }
            });
        } else {
            let payload = format!("\x15{}\r", prompt.trim());
            let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
            w.write_all(payload.as_bytes())?;
            w.flush()?;
        }
        Ok(())
    }

    /// Send a minimal PTY nudge (plain Enter) to trigger a new turn.
    pub async fn nudge_inbox(&self) -> Result<()> {
        let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
        w.write_all(b"\r")?;
        w.flush()?;
        Ok(())
    }

    /// Inject a prompt with echo verification, regardless of CLI type.
    ///
    /// Writes the text bytes (no Ctrl-U pre-clear: the caller is responsible
    /// for ensuring the prompt is in an empty state) and then arms
    /// [`Pane::check_ink_echo_submit`] so the Enter is sent only after the
    /// typed text is observed in the terminal viewport. A 3 s fallback timer
    /// guarantees Enter is delivered even if the echo never appears.
    ///
    /// This is the production path used by stuck-supervisor recovery: it
    /// closes the open-loop "wrote bytes ⇒ assumed delivered" assumption that
    /// causes unsent messages to stack up in Claude Code's multi-line input
    /// when a plain `\r` is interpreted as a newline rather than submit.
    pub async fn inject_prompt_echo_verified(&self, prompt: &str) -> Result<()> {
        let text = prompt.trim();
        {
            let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
            w.write_all(text.as_bytes())?;
            w.flush()?;
        }
        let needle: String = text
            .chars()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let generation = self
            .ink_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let deadline = Instant::now() + Duration::from_secs(3);
        if let Ok(mut pending) = self.pending_ink.lock() {
            *pending = Some((needle, deadline, generation));
        }

        let writer = self.writer.clone();
        let pending_state = Arc::clone(&self.pending_ink);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let still_ours = pending_state
                .lock()
                .map(|p| {
                    p.as_ref()
                        .map(|(_, _, g)| *g == generation)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if still_ours {
                tracing::warn!(
                    generation,
                    "Echo-verified inject fallback: viewport scan did not find typed \
                     text, sending Enter via guaranteed timer"
                );
                let mut w = writer.lock().expect("PTY writer mutex poisoned");
                let _ = w.write_all(b"\r");
                let _ = w.flush();
                if let Ok(mut p) = pending_state.lock() {
                    *p = None;
                }
            }
        });
        Ok(())
    }

    /// Write raw bytes to the PTY.
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().expect("PTY writer mutex poisoned");
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }
}

impl Pane {
    /// Poll for the next PTY event (non-blocking). Returns output, exit, or error.
    pub fn poll(&mut self) -> Option<PtyEvent> {
        let event = match &mut self.backend {
            PaneBackend::Pty(pty) => pty.try_recv(),
            PaneBackend::None => None,
        }?;

        match &event {
            PtyEvent::Output(data) => {
                self.record_output_activity();
                trace_supervisor_bytes(&self.id, self.kind(), "poll.raw", data);
                let feed_data = self.prepare_output_for_feed(data);
                trace_supervisor_bytes(&self.id, self.kind(), "poll.feed", &feed_data);
                if let Err(e) = self.feed_pty_output(&feed_data) {
                    tracing::warn!("Failed to feed data to terminal: {}", e);
                }
            }
            PtyEvent::CursorPositionRequested => {
                self.respond_to_cursor_position_request();
            }
            PtyEvent::Exited(code) => {
                self.mark_exited(*code);
            }
            PtyEvent::Error(_) => {
                self.mark_exited(None);
            }
        }
        Some(event)
    }

    /// Answer an Ink CPR query with the parser's real cursor position.
    ///
    /// Must be called only after the bytes preceding the query have been
    /// fed to the terminal — otherwise the reported position lags reality
    /// and Ink redraws relative to a stale row, producing the symptoms
    /// the CPR responder exists to fix (ghost glyphs, double-writes,
    /// composer state that disagrees with what the user sees).
    fn respond_to_cursor_position_request(&mut self) {
        let (col, row) = self.cursor_position();
        let payload = format_cursor_position_report(row, col);
        let pty = match &self.backend {
            PaneBackend::Pty(pty) => pty,
            PaneBackend::None => return,
        };
        let writer = pty.writer_handle();
        // After F2 the writer mutex is a sync std mutex; the critical
        // section is a sync `write_all` + `flush` on a `Box<dyn Write>`,
        // so contention is bounded by I/O wall time. `lock()` here is
        // the right call — try_lock would be a needless optimisation.
        let mut w = writer.lock().expect("PTY writer mutex poisoned");
        if let Err(e) = w.write_all(&payload) {
            tracing::debug!("CPR reply write failed: {}", e);
            return;
        }
        let _ = w.flush();
    }

    /// Drain available PTY output into a coalesced buffer, returning
    /// non-output events separately. Bounded by
    /// `DRAIN_OUTPUT_MAX_BYTES_PER_CALL` and
    /// `DRAIN_OUTPUT_MAX_EVENTS_PER_CALL`: whichever cap is hit first,
    /// the loop breaks and leftover events stay in the pane's mpsc
    /// channel for the next tick. See § F8b in
    /// `tmp/tick-latency/GOAL_PROMPT.md` for the rationale.
    pub fn drain_output(&mut self) -> (Vec<u8>, Vec<PtyEvent>) {
        // Classify events without touching `self` (so the inner loop is
        // unit-testable as `drain_events_with_budget`). Side effects
        // (record_output_activity, mark_exited) are applied as a second
        // pass over the result.
        let try_recv = |backend: &mut PaneBackend| -> Option<PtyEvent> {
            match backend {
                PaneBackend::Pty(pty) => pty.try_recv(),
                PaneBackend::None => None,
            }
        };
        let DrainBudgetResult {
            coalesced,
            pending_cpr,
            output_event_count,
            other_events: drained_other,
        } = drain_events_with_budget(
            || try_recv(&mut self.backend),
            DRAIN_OUTPUT_MAX_BYTES_PER_CALL,
            DRAIN_OUTPUT_MAX_EVENTS_PER_CALL,
        );

        // Per pre-budget behaviour: one `record_output_activity` per
        // Output event drained. Tracing collapses to a single
        // `drain.raw.coalesced` emit below; the per-chunk
        // `drain.raw.chunk` trace was diagnostic-only and lost here
        // (replaceable by per-event tracing in the helper if needed).
        for _ in 0..output_event_count {
            self.record_output_activity();
        }

        // Apply exit/error side effects and forward non-output events
        // to the caller, matching pre-budget shape.
        let mut other_events = Vec::with_capacity(drained_other.len());
        for event in drained_other {
            match &event {
                PtyEvent::Exited(code) => self.mark_exited(*code),
                PtyEvent::Error(_) => self.mark_exited(None),
                _ => {}
            }
            other_events.push(event);
        }

        if !coalesced.is_empty() {
            trace_supervisor_bytes(&self.id, self.kind(), "drain.raw.coalesced", &coalesced);
            let feed_data = self.prepare_output_for_feed(&coalesced);
            trace_supervisor_bytes(&self.id, self.kind(), "drain.feed", &feed_data);
            if let Err(e) = self.feed_pty_output(&feed_data) {
                tracing::warn!(
                    "Failed to feed {} bytes to terminal: {}",
                    feed_data.len(),
                    e
                );
            }
        }

        // Reply AFTER feeding so the cursor position the parser reports
        // reflects bytes that arrived in the same drain as the query.
        if pending_cpr {
            self.respond_to_cursor_position_request();
        }

        // Echo detection for Ink-based PTY CLIs (Codex/OpenCode/Junie): after feeding output
        // to the terminal, check if pending text has appeared on screen. If so,
        // send Enter to submit it. This replaces the fragile fixed-delay approach.
        self.check_ink_echo_submit();

        (coalesced, other_events)
    }

    /// Check if a pending Ink echo has appeared in the terminal viewport.
    /// If the needle text is found (or the deadline has passed), send Enter.
    ///
    /// Fast path: when no inject is pending (the common case), this is just
    /// a mutex lock + `is_none()` check — no viewport scanning occurs.
    pub(super) fn check_ink_echo_submit(&mut self) {
        let should_submit = {
            let pending = match self.pending_ink_submit.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            let (needle, deadline, _gen) = match pending.as_ref() {
                Some(v) => v,
                None => return,
            };

            if Instant::now() >= *deadline {
                // Fallback: deadline passed, submit anyway
                true
            } else {
                // Scan visible viewport rows for the needle text
                let mut found = false;
                for row in 0..self.rows {
                    if let Ok(text) = self.dump_row(row)
                        && text.contains(needle.as_str())
                    {
                        found = true;
                        break;
                    }
                }
                found
            }
        };

        if should_submit {
            // Send Enter via synchronous write on the PTY writer handle.
            // Only clear pending state AFTER successfully writing — if
            // try_lock loses to a concurrent sync writer, leave the state
            // so the next drain_output tick retries. The spawned tokio
            // fallback timer (in inject_prompt) provides a guaranteed
            // delivery path.
            let should_clear = match &self.backend {
                PaneBackend::Pty(pty) => {
                    let writer = pty.writer_handle();
                    if let Ok(mut w) = writer.try_lock() {
                        let _ = w.write_all(b"\r");
                        let _ = w.flush();
                        tracing::debug!(
                            "Ink echo: sent Enter via fast-path (echo detected or deadline)"
                        );
                        true
                    } else {
                        tracing::debug!(
                            "Ink echo: try_lock failed, deferring to next tick or fallback timer"
                        );
                        false
                    }
                }
                // No PTY backend (e.g. Director pane) — nothing to write,
                // clear state to avoid infinite retry.
                PaneBackend::None => true,
            };
            if should_clear && let Ok(mut pending) = self.pending_ink_submit.lock() {
                *pending = None;
            }
        }
    }

    /// Write raw bytes to the PTY.
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        match &self.backend {
            PaneBackend::Pty(pty) => {
                pty.write(data).await?;
                Ok(())
            }
            PaneBackend::None => Err(Error::pty("Pane has no backend")),
        }
    }

    // ACP prompt delivery has been moved to Mux::deliver_prompt() which routes
    // through the AgentGateway for ACP-capable agents. PTY-backed agents never
    // receive ACP JSON-RPC — they use CLI-specific text injection instead.

    /// Write a line of text followed by a newline to the PTY.
    pub async fn send_line(&self, line: &str) -> Result<()> {
        match &self.backend {
            PaneBackend::Pty(pty) => {
                pty.send_line(line).await?;
                Ok(())
            }
            PaneBackend::None => Err(Error::pty("Pane has no backend")),
        }
    }

    /// Clone the PTY writer handle for non-blocking dispatch.
    pub fn pty_writer_handle(&self) -> Option<Arc<Mutex<Box<dyn Write + Send>>>> {
        match &self.backend {
            PaneBackend::Pty(pty) => Some(pty.writer_handle()),
            PaneBackend::None => None,
        }
    }

    /// Build a cloneable injector handle for this pane.
    pub fn injector_handle(&self) -> Option<PaneInjector> {
        match &self.backend {
            PaneBackend::Pty(_) => Some(PaneInjector {
                pane_id: self.id.clone(),
                cli_type: self.cli_type.clone(),
                writer: self.pty_writer_handle()?,
                ink_generation: Arc::clone(&self.ink_submit_generation),
                pending_ink: Arc::clone(&self.pending_ink_submit),
            }),
            PaneBackend::None => None,
        }
    }

    /// Inject a prompt into the pane using CLI-specific key sequences.
    pub async fn inject_prompt(&self, prompt: &str) -> Result<()> {
        match self.injector_handle() {
            Some(injector) => injector.inject_prompt(prompt).await,
            None => Err(Error::pty("Pane has no backend")),
        }
    }

    /// Send a minimal PTY nudge to trigger a new turn without clearing input.
    ///
    /// Used when the actual message content was delivered via Teams inbox file.
    /// The agent reads the inbox at turn start, so we just need to trigger a
    /// turn — no visible text required. Claude inbox nudges only happen when
    /// the pane is at a quiet, empty prompt, so a plain Enter is safe.
    pub async fn nudge_inbox(&self) -> Result<()> {
        match self.injector_handle() {
            Some(injector) => injector.nudge_inbox().await,
            None => Err(Error::pty("Pane has no backend")),
        }
    }

    /// Inject a prompt with viewport-echo verification before submit.
    ///
    /// See `PaneInjector::inject_prompt_echo_verified` for details. The
    /// caller must ensure the input box is in an empty state — otherwise the
    /// typed text will append to whatever draft is already there.
    pub async fn inject_prompt_echo_verified(&self, prompt: &str) -> Result<()> {
        match self.injector_handle() {
            Some(injector) => injector.inject_prompt_echo_verified(prompt).await,
            None => Err(Error::pty("Pane has no backend")),
        }
    }

    /// Send Ctrl-C (interrupt) to the PTY process.
    pub async fn interrupt(&self) -> Result<()> {
        match &self.backend {
            PaneBackend::Pty(pty) => {
                pty.interrupt().await?;
                Ok(())
            }
            PaneBackend::None => Err(Error::pty("Pane has no backend")),
        }
    }

    /// Kill the PTY process immediately.
    pub fn kill(&mut self) {
        match &mut self.backend {
            PaneBackend::Pty(pty) => pty.kill(),
            PaneBackend::None => {}
        }
    }

    /// Start recording pane output to a file.
    pub async fn start_recording(
        &mut self,
        session_id: impl Into<String>,
        config: WriterConfig,
    ) -> Result<()> {
        if self.recorder.is_some() {
            return Err(Error::recording("Recording already in progress"));
        }

        let writer = brehon_recording::RecordingWriter::new(
            self.cols,
            self.rows,
            self.id.clone(),
            session_id.into(),
            self.kind.as_str(),
            config,
        )
        .await
        .map_err(|e| Error::recording(e.to_string()))?;

        self.recorder = Some(Arc::new(tokio::sync::Mutex::new(writer)));

        self.generate_keyframe().await?;

        tracing::info!("Started recording for pane {}", self.id);
        Ok(())
    }

    /// Stop recording and return the output file path, if recording was active.
    pub async fn stop_recording(&mut self) -> Result<Option<PathBuf>> {
        if let Some(recorder) = self.recorder.take() {
            let writer = match Arc::try_unwrap(recorder) {
                Ok(mutex) => mutex.into_inner(),
                Err(_) => return Err(Error::recording("Recording still in use")),
            };
            let path = writer.file_path().clone();
            writer
                .close()
                .await
                .map_err(|e| Error::recording(e.to_string()))?;
            tracing::info!(
                "Stopped recording for pane {}, saved to {:?}",
                self.id,
                path
            );
            Ok(Some(path))
        } else {
            Ok(None)
        }
    }

    async fn generate_keyframe(&mut self) -> Result<()> {
        if let Some(ref recorder) = self.recorder {
            let mut lines = Vec::new();
            for row in 0..self.rows {
                let text = self
                    .terminal
                    .dump_screen_row(row as u32)
                    .unwrap_or_default();
                lines.push(text);
            }
            let content = lines.join("\n").into_bytes();

            let mut writer = recorder.lock().await;
            writer
                .write_keyframe(content)
                .await
                .map_err(|e| Error::recording(e.to_string()))?;
        }
        Ok(())
    }

    /// Record a chunk of output data to the active recording, if any.
    pub async fn record_output(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref recorder) = self.recorder {
            let writer = recorder.lock().await;
            writer
                .write_output(data)
                .await
                .map_err(|e| Error::recording(e.to_string()))?;
        }
        Ok(())
    }

    /// Whether this pane is currently recording output.
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if let Some(path) = &self.notify_socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// Helper that mints the (slightly artificial) recv closure over a
    /// pre-staged `VecDeque<PtyEvent>`. Matches the shape of the closure
    /// `Pane::drain_output` constructs over its real backend.
    fn recv_from(
        queue: std::cell::RefCell<std::collections::VecDeque<PtyEvent>>,
    ) -> impl Fn() -> Option<PtyEvent> {
        move || queue.borrow_mut().pop_front()
    }

    /// Verifies the F8b byte-budget invariant: a single drain call
    /// stops near `max_bytes` (with at most one event's worth of
    /// overshoot, per the documented semantics) and a follow-up call
    /// returns the remaining queued bytes — proving leftover events
    /// stay queued for the next tick instead of being processed in
    /// one shot.
    #[test]
    fn drain_events_with_budget_respects_byte_cap() {
        const CHUNK: usize = 16 * 1024;
        const TOTAL_CHUNKS: usize = 32; // 512 KiB total, > 256 KiB cap
        let queue: std::cell::RefCell<std::collections::VecDeque<PtyEvent>> =
            std::cell::RefCell::new(std::collections::VecDeque::new());
        for _ in 0..TOTAL_CHUNKS {
            queue
                .borrow_mut()
                .push_back(PtyEvent::Output(vec![b'x'; CHUNK]));
        }
        let recv = recv_from(queue);

        let first = drain_events_with_budget(
            &recv,
            DRAIN_OUTPUT_MAX_BYTES_PER_CALL,
            DRAIN_OUTPUT_MAX_EVENTS_PER_CALL,
        );

        // First call should have stopped at or just past the cap.
        assert!(
            first.coalesced.len() >= DRAIN_OUTPUT_MAX_BYTES_PER_CALL,
            "first drain returned {} bytes, expected >= cap {}",
            first.coalesced.len(),
            DRAIN_OUTPUT_MAX_BYTES_PER_CALL
        );
        assert!(
            first.coalesced.len() <= DRAIN_OUTPUT_MAX_BYTES_PER_CALL + CHUNK,
            "first drain overshot by more than one event's worth: \
             got {}, cap+chunk = {}",
            first.coalesced.len(),
            DRAIN_OUTPUT_MAX_BYTES_PER_CALL + CHUNK
        );
        assert!(first.output_event_count > 0);
        assert!(first.other_events.is_empty());

        // A second call must still find queued bytes — the budget
        // deferred work to the next tick rather than starving it.
        let second = drain_events_with_budget(
            &recv,
            DRAIN_OUTPUT_MAX_BYTES_PER_CALL,
            DRAIN_OUTPUT_MAX_EVENTS_PER_CALL,
        );
        assert!(
            !second.coalesced.is_empty(),
            "second drain returned no bytes — first call drained beyond cap"
        );
        assert_eq!(
            first.coalesced.len()
                + second.coalesced.len()
                + drain_events_with_budget(
                    &recv,
                    DRAIN_OUTPUT_MAX_BYTES_PER_CALL,
                    DRAIN_OUTPUT_MAX_EVENTS_PER_CALL,
                )
                .coalesced
                .len(),
            CHUNK * TOTAL_CHUNKS,
            "bytes lost or duplicated across budgeted drains"
        );
    }

    /// Verifies the event-count cap stops the loop even when each
    /// event is tiny — protects against a burst of CPR queries or
    /// empty Output events monopolising the tick without ever
    /// tripping the byte cap.
    #[test]
    fn drain_events_with_budget_respects_event_cap() {
        let queue: std::cell::RefCell<std::collections::VecDeque<PtyEvent>> =
            std::cell::RefCell::new(std::collections::VecDeque::new());
        // 200 tiny events, each well under the byte cap and not
        // accumulating bytes (CPR queries).
        for _ in 0..200 {
            queue
                .borrow_mut()
                .push_back(PtyEvent::CursorPositionRequested);
        }
        let recv = recv_from(queue);

        let first = drain_events_with_budget(&recv, 1024 * 1024, 64);
        // CPR events don't contribute bytes, so the byte cap is
        // unreachable; only the event cap can stop us.
        assert_eq!(first.coalesced.len(), 0);
        assert!(first.pending_cpr);
        // Confirm by exhausting remaining queue with one more call
        // bounded the same way — leftovers should still be there.
        let second = drain_events_with_budget(&recv, 1024 * 1024, 64);
        assert!(second.pending_cpr);
        let third = drain_events_with_budget(&recv, 1024 * 1024, 64);
        // 64 + 64 + 64 = 192, so the third call should still have
        // pulled events even if it didn't exhaust the queue.
        assert!(third.pending_cpr || drain_events_with_budget(&recv, 1024 * 1024, 64).pending_cpr);
    }

    #[test]
    fn backpressure_drain_budget_leaves_excess_output_queued() {
        let queue: std::cell::RefCell<std::collections::VecDeque<PtyEvent>> =
            std::cell::RefCell::new(std::collections::VecDeque::new());
        queue
            .borrow_mut()
            .push_back(PtyEvent::Output(b"a".to_vec()));
        queue
            .borrow_mut()
            .push_back(PtyEvent::Output(b"b".to_vec()));
        let recv = recv_from(queue);

        let first = drain_events_with_budget(&recv, 1024, 1);
        assert_eq!(first.coalesced, b"a");

        let second = drain_events_with_budget(&recv, 1024, 1);
        assert_eq!(second.coalesced, b"b");
    }

    /// Exit / Error events are forwarded verbatim and do NOT get
    /// coalesced into the byte stream.
    #[test]
    fn drain_events_with_budget_forwards_other_events() {
        let queue: std::cell::RefCell<std::collections::VecDeque<PtyEvent>> =
            std::cell::RefCell::new(std::collections::VecDeque::new());
        queue
            .borrow_mut()
            .push_back(PtyEvent::Output(b"hi".to_vec()));
        queue.borrow_mut().push_back(PtyEvent::Exited(Some(0)));
        queue.borrow_mut().push_back(PtyEvent::Error("boom".into()));
        let recv = recv_from(queue);

        let result = drain_events_with_budget(&recv, 1024, 64);
        assert_eq!(result.coalesced, b"hi");
        assert_eq!(result.other_events.len(), 2);
        assert!(matches!(result.other_events[0], PtyEvent::Exited(Some(0))));
        assert!(matches!(result.other_events[1], PtyEvent::Error(_)));
    }
}
