//! Mux-level integration for the supervisor-only Panesmith dogfood path.

use std::time::Duration;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::pane::panesmith_shim::BrehonPanesmithEventKind;
use crate::pane::spawn::{
    PRE_SUBMIT_INTER_INTERRUPT_DELAY, PRE_SUBMIT_SETTLE_DELAY, uses_delayed_submit_injection,
    uses_ink_echo_injection, uses_pre_submit_interrupt_reset,
};
use crate::pane::{Pane, PaneKind};

use super::Mux;
use super::types::MuxEvent;

impl Mux {
    /// Return the latest owned Panesmith snapshot for a Brehon pane id.
    pub fn panesmith_snapshot(&self, pane_id: &str) -> Option<&panesmith::OwnedPaneSnapshot> {
        self.panesmith.snapshot(pane_id)
    }

    /// Return the latest owned Panesmith scrollback for a Brehon pane id.
    pub fn panesmith_scrollback(
        &self,
        pane_id: &str,
    ) -> Option<&panesmith::OwnedScrollbackSnapshot> {
        self.panesmith.scrollback(pane_id)
    }

    /// Whether a Brehon pane currently has a Panesmith-managed PTY/surface.
    pub fn is_panesmith_managed(&self, pane_id: &str) -> bool {
        self.panesmith.contains(pane_id)
    }

    pub(crate) fn spawn_panesmith_supervisor_for_pane(&mut self, pane: &mut Pane) -> Result<()> {
        if pane.kind() != &PaneKind::Supervisor {
            return Err(Error::pty(format!(
                "Pane '{}' is not a supervisor and cannot be Panesmith-managed",
                pane.id()
            )));
        }
        let config = pane
            .pty_spawn_config
            .as_ref()
            .ok_or_else(|| Error::pty(format!("Pane '{}' has no PTY spawn config", pane.id())))?;
        let pane_id = pane.id().to_string();
        let title = pane.title().to_string();

        self.panesmith.spawn_supervisor(&pane_id, config, &title)?;
        pane.set_panesmith_managed(true);
        pane.set_tool_executing(true);
        pane.set_last_output_at(Instant::now());
        Ok(())
    }

    pub(crate) fn restart_panesmith_supervisor_for_existing_pane(
        &mut self,
        pane_id: &str,
    ) -> Result<()> {
        let (config, title) = {
            let pane = self
                .panes
                .get(pane_id)
                .ok_or_else(|| Error::pane_not_found(pane_id))?;
            if pane.kind() != &PaneKind::Supervisor {
                return Err(Error::pty(format!(
                    "Pane '{pane_id}' is not a supervisor and cannot be Panesmith-managed"
                )));
            }
            let config =
                pane.pty_spawn_config.as_ref().cloned().ok_or_else(|| {
                    Error::pty(format!("Pane '{pane_id}' has no PTY spawn config"))
                })?;
            (config, pane.title().to_string())
        };

        self.panesmith.spawn_supervisor(pane_id, &config, &title)?;
        if let Some(pane) = self.panes.get_mut(pane_id) {
            pane.set_panesmith_managed(true);
            pane.set_tool_executing(true);
            pane.set_last_output_at(Instant::now());
        }
        Ok(())
    }

    pub(crate) fn send_panesmith_input_bytes(
        &mut self,
        pane_id: &str,
        data: &[u8],
    ) -> Result<bool> {
        if !self.panesmith.contains(pane_id) {
            return Ok(false);
        }
        if let Some(transaction) = panesmith_transaction_for_input_bytes(data) {
            let outcome = self
                .panesmith
                .send_input_transaction(pane_id, transaction)?;
            ensure_panesmith_mux_outcome("paste input", &outcome)?;
        } else {
            self.panesmith.send_input_bytes(pane_id, data)?;
        }
        let events = self.drain_panesmith_events_to_mux();
        self.pending_panesmith_events.extend(events);
        Ok(true)
    }

    pub(crate) fn send_panesmith_input_transaction(
        &mut self,
        pane_id: &str,
        transaction: panesmith::InputTransaction,
    ) -> Result<Option<panesmith::InputOutcome>> {
        if !self.panesmith.contains(pane_id) {
            return Ok(None);
        }
        let outcome = self
            .panesmith
            .send_input_transaction(pane_id, transaction)?;
        let events = self.drain_panesmith_events_to_mux();
        self.pending_panesmith_events.extend(events);
        Ok(Some(outcome))
    }

    pub(crate) async fn send_panesmith_prompt_transaction(
        &mut self,
        pane_id: &str,
        prompt: &str,
    ) -> Result<Option<panesmith::InputOutcome>> {
        if !self.panesmith.contains(pane_id) {
            return Ok(None);
        }

        let cli_type = self
            .panes
            .get(pane_id)
            .map(|pane| pane.cli_type().clone())
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        let text = prompt.trim().to_string();
        let mut combined = panesmith::InputOutcome::default();

        if uses_pre_submit_interrupt_reset(&cli_type) {
            merge_panesmith_input_outcome(
                &mut combined,
                self.send_panesmith_input_transaction(
                    pane_id,
                    panesmith::InputTransaction::interrupt(),
                )?
                .expect("managed pane should return an input outcome"),
            );
            if !combined.errors.is_empty() {
                return Ok(Some(combined));
            }
            tokio::time::sleep(PRE_SUBMIT_INTER_INTERRUPT_DELAY).await;
            merge_panesmith_input_outcome(
                &mut combined,
                self.send_panesmith_input_transaction(
                    pane_id,
                    panesmith::InputTransaction::interrupt(),
                )?
                .expect("managed pane should return an input outcome"),
            );
            if !combined.errors.is_empty() {
                return Ok(Some(combined));
            }
            tokio::time::sleep(PRE_SUBMIT_SETTLE_DELAY).await;
        }

        if uses_delayed_submit_injection(&cli_type) {
            merge_panesmith_input_outcome(
                &mut combined,
                self.send_panesmith_input_transaction(
                    pane_id,
                    panesmith::InputTransaction::clear_input(),
                )?
                .expect("managed pane should return an input outcome"),
            );
            if !combined.errors.is_empty() {
                return Ok(Some(combined));
            }
            merge_panesmith_input_outcome(
                &mut combined,
                self.send_panesmith_input_transaction(
                    pane_id,
                    panesmith::InputTransaction::insert_text(text),
                )?
                .expect("managed pane should return an input outcome"),
            );
            if !combined.errors.is_empty() {
                return Ok(Some(combined));
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
            merge_panesmith_input_outcome(
                &mut combined,
                self.send_panesmith_input_transaction(pane_id, panesmith_enter_transaction())?
                    .expect("managed pane should return an input outcome"),
            );
            return Ok(Some(combined));
        }

        let transaction = if uses_ink_echo_injection(&cli_type) {
            panesmith::InputTransaction::submit_text(text.clone()).with_verification(
                panesmith::InputVerification::EchoContains {
                    needle: panesmith_echo_needle(&text),
                    timeout: Duration::from_secs(3),
                },
            )
        } else {
            merge_panesmith_input_outcome(
                &mut combined,
                self.send_panesmith_input_transaction(
                    pane_id,
                    panesmith::InputTransaction::clear_input(),
                )?
                .expect("managed pane should return an input outcome"),
            );
            if !combined.errors.is_empty() {
                return Ok(Some(combined));
            }
            panesmith::InputTransaction::submit_text(text)
        };

        merge_panesmith_input_outcome(
            &mut combined,
            self.send_panesmith_input_transaction(pane_id, transaction)?
                .expect("managed pane should return an input outcome"),
        );
        Ok(Some(combined))
    }

    pub fn resize_panesmith_pane(&mut self, pane_id: &str, rows: u16, cols: u16) -> Result<bool> {
        self.panesmith.resize(pane_id, rows, cols)
    }

    pub(crate) fn kill_panesmith_pane(&mut self, pane_id: &str) -> Result<bool> {
        self.panesmith.kill_and_forget(pane_id)
    }

    pub fn attach_panesmith_pane_blocking<Terminal, Control>(
        &mut self,
        pane_id: &str,
        options: panesmith::AttachOptions,
        terminal: &mut Terminal,
        control: &mut Control,
    ) -> Result<panesmith::PaneAttachOutcome>
    where
        Terminal: panesmith::PaneAttachTerminal,
        Control: panesmith::PaneAttachTerminalControl,
    {
        if !self.panesmith.contains(pane_id) {
            return Err(Error::pane_not_found(pane_id));
        }

        let pending = self.drain_panesmith_events_to_mux();
        self.pending_panesmith_events.extend(pending);

        let outcome = self
            .panesmith
            .attach_blocking(pane_id, options, terminal, control);

        let pending = self.drain_panesmith_events_to_mux();
        self.pending_panesmith_events.extend(pending);

        outcome
    }

    pub(crate) fn drain_panesmith_events_to_mux(&mut self) -> Vec<MuxEvent> {
        let mirrored = self.panesmith.drain_events();
        let mut mux_events = Vec::new();

        for event in mirrored {
            tracing::trace!(
                pane = %event.pane_id,
                panesmith_pane_id = event.panesmith_pane_id.get(),
                seq = event.seq,
                kind = ?event.kind,
                "mirrored Panesmith pane event"
            );

            match event.kind {
                BrehonPanesmithEventKind::Output { .. }
                | BrehonPanesmithEventKind::SurfaceChanged => {
                    if let Some(pane) = self.panes.get_mut(&event.pane_id) {
                        pane.record_output_activity();
                        pane.bump_render_generation();
                        mux_events.push(MuxEvent::PaneOutput {
                            pane_id: event.pane_id.clone(),
                            data: Vec::new(),
                            generation: pane.current_generation(),
                        });
                    }
                }
                BrehonPanesmithEventKind::Exited { code } => {
                    if let Some(pane) = self.panes.get_mut(&event.pane_id) {
                        pane.mark_exited(code);
                    }
                    mux_events.push(MuxEvent::PaneExited {
                        pane_id: event.pane_id,
                        exit_code: code,
                    });
                }
                BrehonPanesmithEventKind::Resized { .. }
                | BrehonPanesmithEventKind::InputSent { .. }
                | BrehonPanesmithEventKind::Spawned
                | BrehonPanesmithEventKind::StateChanged
                | BrehonPanesmithEventKind::Other(_) => {}
                BrehonPanesmithEventKind::Error { message } => {
                    tracing::warn!(
                        pane = %event.pane_id,
                        error = %message,
                        "Panesmith pane error"
                    );
                }
            }
        }

        mux_events
    }
}

fn panesmith_echo_needle(text: &str) -> String {
    text.chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn panesmith_transaction_for_input_bytes(data: &[u8]) -> Option<panesmith::InputTransaction> {
    const BRACKETED_PASTE_BEGIN: &[u8] = b"\x1b[200~";
    const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

    let inner = data
        .strip_prefix(BRACKETED_PASTE_BEGIN)?
        .strip_suffix(BRACKETED_PASTE_END)?;
    let text = std::str::from_utf8(inner).ok()?;
    Some(panesmith::InputTransaction::insert_text(text.to_string()))
}

pub(super) fn panesmith_enter_transaction() -> panesmith::InputTransaction {
    panesmith::InputTransaction::key_chord(panesmith::KeyInput::new(
        panesmith::KeyCode::Enter,
        panesmith::KeyModifiers::default(),
        panesmith::KeyEventKind::Press,
    ))
}

fn ensure_panesmith_mux_outcome(operation: &str, outcome: &panesmith::InputOutcome) -> Result<()> {
    if outcome.errors.is_empty() {
        Ok(())
    } else {
        Err(Error::pty(format!(
            "Panesmith {operation} failed: {:?}",
            outcome.errors
        )))
    }
}

fn merge_panesmith_input_outcome(
    target: &mut panesmith::InputOutcome,
    next: panesmith::InputOutcome,
) {
    target.bytes_sent += next.bytes_sent;
    target.echoed |= next.echoed;
    target.submitted |= next.submitted;
    target.timed_out |= next.timed_out;
    target.child_exited |= next.child_exited;
    target.errors.extend(next.errors);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_bytes_become_panesmith_insert_text_intent() {
        let transaction = panesmith_transaction_for_input_bytes(b"\x1b[200~hello\nworld\x1b[201~")
            .expect("bracketed paste bytes should be recognized");

        assert!(matches!(
            transaction.intent,
            panesmith::InputIntent::InsertText(ref text) if text == "hello\nworld"
        ));
    }

    #[test]
    fn non_paste_bytes_stay_raw_passthrough() {
        assert!(panesmith_transaction_for_input_bytes(b"hello\r").is_none());
    }
}
