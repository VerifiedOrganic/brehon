//! Runtime side-channel mapping for mux events.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brehon_ports::RuntimeEventSink;
use brehon_types::{
    ActivityObservedEvent, PaneExitedEvent, PaneOutputEvent, PaneSpawnedEvent,
    PaneStateChangedEvent, PromptDeliveredEvent, PromptQueuedEvent, PromptRejectedEvent,
    RuntimeActivityKind, RuntimeEvent, RuntimeEventKind, RuntimeEventMeta, RuntimePaneKind,
    RuntimePaneState, RuntimeSource,
};

use super::Mux;
use super::types::{AsyncGatewayPromptDeliveryError, MuxEvent, PromptDeliveryAttempt};
use crate::pane::{ActivityKind, Generation, PaneKind, PaneState};

impl Mux {
    /// Install a side-channel sink for runtime events.
    pub fn set_runtime_event_sink(&mut self, sink: Arc<dyn RuntimeEventSink>) {
        self.runtime_event_sink = Some(sink);
    }

    /// Disable runtime side-channel publication.
    pub fn clear_runtime_event_sink(&mut self) {
        self.runtime_event_sink = None;
    }

    pub(crate) fn publish_runtime_event_for_mux_event(&self, event: &MuxEvent) {
        let Some(runtime_event) = self.runtime_event_for_mux_event(event) else {
            return;
        };
        self.publish_runtime_event(runtime_event);
    }

    pub(crate) fn publish_runtime_event(&self, runtime_event: RuntimeEvent) {
        let Some(sink) = self.runtime_event_sink.clone() else {
            return;
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(err) = sink.publish(runtime_event).await {
                        tracing::warn!(error = %err, "Failed to publish runtime event");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Runtime event sink configured but no Tokio runtime is active"
                );
            }
        }
    }

    pub(crate) fn publish_runtime_pane_spawned(&self, pane_id: &str) {
        self.publish_runtime_event_for_mux_event(&MuxEvent::PaneAdded {
            pane_id: pane_id.to_string(),
        });
    }

    pub(crate) fn publish_runtime_pane_state_changed(
        &self,
        pane_id: &str,
        generation: Generation,
        previous: Option<RuntimePaneState>,
        current: RuntimePaneState,
        reason: Option<String>,
    ) {
        self.publish_runtime_event(RuntimeEvent::new(
            self.runtime_meta(pane_id, generation),
            RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                previous,
                current,
                reason,
            }),
        ));
    }

    pub(crate) fn runtime_pane_state_for_state(state: &PaneState) -> RuntimePaneState {
        match state {
            PaneState::Ready { .. } => RuntimePaneState::Ready,
            PaneState::Busy { .. } => RuntimePaneState::Busy,
            PaneState::Dead { .. } => RuntimePaneState::Dead,
        }
    }

    pub(crate) fn runtime_state_change(
        previous: Option<RuntimePaneState>,
        current: Option<&PaneState>,
        reason: impl Into<String>,
    ) -> Option<(Option<RuntimePaneState>, RuntimePaneState, String)> {
        let current = current.map(Self::runtime_pane_state_for_state)?;
        if previous.as_ref() == Some(&current) {
            return None;
        }
        Some((previous, current, reason.into()))
    }

    pub(crate) fn runtime_event_for_mux_event(&self, event: &MuxEvent) -> Option<RuntimeEvent> {
        match event {
            MuxEvent::PaneOutput {
                pane_id,
                data,
                generation,
            } => Some(RuntimeEvent::new(
                self.runtime_meta(pane_id, *generation),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: data.clone(),
                    text: None,
                }),
            )),
            MuxEvent::PaneExited { pane_id, exit_code } => Some(RuntimeEvent::new(
                self.runtime_meta(pane_id, self.current_generation_or_default(pane_id)),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: *exit_code,
                    reason: None,
                }),
            )),
            MuxEvent::PaneAdded { pane_id } => Some(RuntimeEvent::new(
                self.runtime_meta(pane_id, self.current_generation_or_default(pane_id)),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: self
                        .panes
                        .get(pane_id)
                        .map(|pane| runtime_pane_kind(pane.kind()))
                        .unwrap_or(RuntimePaneKind::Unknown),
                    title: Some(pane_id.clone()),
                }),
            )),
            MuxEvent::PaneRemoved { pane_id } => Some(RuntimeEvent::new(
                self.runtime_meta(pane_id, self.current_generation_or_default(pane_id)),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: None,
                    reason: Some("removed".to_string()),
                }),
            )),
            MuxEvent::ActivityEvent {
                pane_id,
                entry,
                generation,
            } => Some(RuntimeEvent::new(
                self.runtime_meta(pane_id, *generation),
                RuntimeEventKind::ActivityObserved(ActivityObservedEvent {
                    kind: runtime_activity_kind(entry.kind, entry.status.as_deref()),
                    description: entry.message.clone(),
                }),
            )),
            MuxEvent::AsyncGatewayPromptDeliveryCompleted {
                pane_id,
                generation,
                result,
                ..
            }
            | MuxEvent::AsyncTeamsPromptDeliveryCompleted {
                pane_id,
                generation,
                result,
                ..
            } => runtime_prompt_delivery_event(self.runtime_meta(pane_id, *generation), result),
            MuxEvent::ActivityFlush {
                pane_id,
                generation,
            } => Some(RuntimeEvent::new(
                self.runtime_meta(pane_id, *generation),
                RuntimeEventKind::ActivityObserved(ActivityObservedEvent {
                    kind: RuntimeActivityKind::Output,
                    description: Some("flush".to_string()),
                }),
            )),
            MuxEvent::FocusChanged { .. }
            | MuxEvent::TaskContextChanged { .. }
            | MuxEvent::ReviewContextChanged { .. } => None,
        }
    }

    pub(crate) fn runtime_meta(&self, pane_id: &str, generation: Generation) -> RuntimeEventMeta {
        RuntimeEventMeta::new(
            self.session_name
                .as_deref()
                .unwrap_or("default")
                .to_string(),
            pane_id.to_string(),
            generation.0,
            RuntimeSource::Mux,
            unix_timestamp_ms(),
        )
    }

    pub(crate) fn current_generation_or_default(&self, pane_id: &str) -> Generation {
        self.panes
            .get(pane_id)
            .map(|pane| pane.current_generation())
            .unwrap_or_default()
    }
}

fn runtime_prompt_delivery_event(
    meta: RuntimeEventMeta,
    result: &std::result::Result<PromptDeliveryAttempt, AsyncGatewayPromptDeliveryError>,
) -> Option<RuntimeEvent> {
    match result {
        Ok(PromptDeliveryAttempt::Delivered { prompt_id, .. }) => Some(RuntimeEvent::new(
            meta,
            RuntimeEventKind::PromptDelivered(PromptDeliveredEvent {
                prompt_id: prompt_id.to_string(),
            }),
        )),
        Ok(PromptDeliveryAttempt::Queued {
            prompt_id,
            ahead_of,
        }) => Some(RuntimeEvent::new(
            meta,
            RuntimeEventKind::PromptQueued(PromptQueuedEvent {
                prompt_id: prompt_id.to_string(),
                queue_depth: *ahead_of,
            }),
        )),
        Ok(PromptDeliveryAttempt::Rejected { reason }) => Some(RuntimeEvent::new(
            meta,
            RuntimeEventKind::PromptRejected(PromptRejectedEvent {
                prompt_id: "unknown".to_string(),
                reason: format!("{reason:?}"),
            }),
        )),
        Ok(PromptDeliveryAttempt::AlreadyPresent { .. }) => None,
        Err(err) => Some(RuntimeEvent::new(
            meta,
            RuntimeEventKind::PromptRejected(PromptRejectedEvent {
                prompt_id: "unknown".to_string(),
                reason: err.error.clone(),
            }),
        )),
    }
}

fn runtime_activity_kind(kind: ActivityKind, status: Option<&str>) -> RuntimeActivityKind {
    match kind {
        ActivityKind::Operation => match status {
            Some("completed" | "failed" | "cancelled" | "success" | "ok") => {
                RuntimeActivityKind::OperationCompleted
            }
            _ => RuntimeActivityKind::OperationStarted,
        },
        ActivityKind::Permission => match status {
            Some("resolved" | "approved" | "denied") => RuntimeActivityKind::PermissionResolved,
            _ => RuntimeActivityKind::PermissionRequested,
        },
        ActivityKind::ToolCall => match status {
            Some("completed" | "failed" | "cancelled" | "success" | "ok") => {
                RuntimeActivityKind::ToolCompleted
            }
            _ => RuntimeActivityKind::ToolStarted,
        },
        ActivityKind::Output | ActivityKind::Progress => RuntimeActivityKind::Output,
    }
}

fn runtime_pane_kind(kind: &PaneKind) -> RuntimePaneKind {
    match kind {
        PaneKind::Supervisor => RuntimePaneKind::Supervisor,
        PaneKind::Worker => RuntimePaneKind::Worker,
        PaneKind::Reviewer => RuntimePaneKind::Reviewer,
        PaneKind::Advisor => RuntimePaneKind::Advisor,
        PaneKind::Research => RuntimePaneKind::Research,
        PaneKind::Director => RuntimePaneKind::Director,
        PaneKind::Shell => RuntimePaneKind::Shell,
    }
}

pub(crate) fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
