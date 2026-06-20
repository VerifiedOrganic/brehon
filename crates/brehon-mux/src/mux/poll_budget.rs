//! Per-poll work budgets for the TUI-facing mux drain path.

use super::Mux;
use super::types::{MAX_OUTPUT_BYTES_PER_POLL, MAX_QUEUED_EVENTS_PER_POLL, MuxEvent};
use crate::pty::PtyEvent;

impl Mux {
    pub(super) fn drain_queued_events_for_poll(
        &mut self,
        events: &mut Vec<MuxEvent>,
        total_bytes: &mut usize,
    ) {
        let limit = self
            .max_queued_events_per_poll
            .min(MAX_QUEUED_EVENTS_PER_POLL);
        for _ in 0..limit {
            if *total_bytes >= MAX_OUTPUT_BYTES_PER_POLL {
                break;
            }
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            if !self.apply_queued_event(&event) {
                continue;
            }
            if let MuxEvent::PaneOutput { data, .. } = &event {
                *total_bytes += data.len();
            }
            events.push(event);
        }
    }

    pub(super) fn drain_pane_outputs_for_poll(
        &mut self,
        events: &mut Vec<MuxEvent>,
        total_bytes: &mut usize,
    ) {
        let pane_count = self.panes.len();
        if pane_count == 0 {
            return;
        }

        let start_index = self.next_output_drain_index % pane_count;
        let mut next_output_drain_index = start_index;
        for offset in 0..pane_count {
            if *total_bytes >= MAX_OUTPUT_BYTES_PER_POLL {
                break;
            }
            let index = (start_index + offset) % pane_count;
            next_output_drain_index = (index + 1) % pane_count;
            let Some((id, pane)) = self.panes.get_index_mut(index) else {
                continue;
            };
            let (data, other_events) = pane.drain_output();
            let generation = pane.current_generation();

            if !data.is_empty() {
                *total_bytes += data.len();
                events.push(MuxEvent::PaneOutput {
                    pane_id: id.clone(),
                    data,
                    generation,
                });
            }

            for event in other_events {
                match event {
                    PtyEvent::Exited(code) => {
                        events.push(MuxEvent::PaneExited {
                            pane_id: id.clone(),
                            exit_code: code,
                        });
                    }
                    PtyEvent::Error(err) => {
                        tracing::error!("PTY error in pane {}: {}", id, err);
                    }
                    _ => {}
                }
            }
        }
        self.next_output_drain_index = next_output_drain_index;
    }
}
