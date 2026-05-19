//! Event routing, polling, and the ACP event bridge.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::Mux;
use super::format::{
    format_acp_session_event, normalize_gateway_tool_event, session_event_to_activity_entry,
};
use super::types::{GATEWAY_PROMPT_RETRY_DELAY, MuxEvent, PromptDeliveryAttempt};
use crate::pane::{
    ActivityKind, DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP, Generation, PaneId, PaneState,
    QueuedPrompt,
};
use crate::pty::PtyEvent;
use brehon_types::{PromptId, RuntimePaneState};

impl Mux {
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
                if let Some((previous, current, reason)) = Self::runtime_state_change(
                    previous,
                    pane.pane_state(),
                    "stale activity cleared",
                ) {
                    state_changes.push((pane_id.clone(), generation, previous, current, reason));
                }
            }
            cleared.push((pane_id, stale_tools, operation_stale, still_busy));
        }

        for (pane_id, generation, previous, current, reason) in state_changes {
            self.publish_runtime_pane_state_changed(
                &pane_id,
                generation,
                previous,
                current,
                Some(reason),
            );
        }

        cleared
    }

    fn synthetic_busy_prompt_id(prefix: &str, pane_id: &str) -> PromptId {
        PromptId::new(format!("{prefix}:{pane_id}:{}", uuid::Uuid::new_v4()))
    }

    pub fn mark_gateway_delivery_busy(
        &mut self,
        pane_id: &str,
        prompt_id: PromptId,
        generation: Generation,
    ) {
        let mut state_change = None;
        let now = Instant::now();
        if let Some(pane) = self.panes.get_mut(pane_id) {
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
        if let Some((previous, current, reason)) = state_change {
            self.publish_runtime_pane_state_changed(
                pane_id,
                generation,
                previous,
                current,
                Some(reason),
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
    ) -> Option<(Option<RuntimePaneState>, RuntimePaneState, String)> {
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
                if let Some(pane) = self.panes.get_mut(pane_id)
                    && let Err(err) = pane.append_output(data)
                {
                    tracing::warn!(
                        pane = %pane_id,
                        error = %err,
                        "Failed to append queued pane output"
                    );
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
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.record_output_activity();
                    pane.ensure_activity_buffer();
                    let now = Instant::now();
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
                if let Some((previous, current, reason)) = state_change {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        *generation,
                        previous,
                        current,
                        Some(reason),
                    );
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
                let mut state_change = None;
                match result {
                    Ok(PromptDeliveryAttempt::Delivered {
                        prompt_id,
                        generation: delivered_generation,
                    }) => {
                        if let Some(pane) = self.panes.get_mut(pane_id) {
                            let previous =
                                pane.pane_state().map(Self::runtime_pane_state_for_state);
                            let now = Instant::now();
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
                        self.mark_gateway_delivery_busy(pane_id, prompt_id.clone(), *generation);
                        let inject_after = Instant::now() + GATEWAY_PROMPT_RETRY_DELAY;
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
                            let inject_after = Instant::now() + GATEWAY_PROMPT_RETRY_DELAY;
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
                if let Some((previous, current, reason)) = state_change {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        *generation,
                        previous,
                        current,
                        Some(reason),
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
                if let Some((previous, current, reason)) = state_change {
                    self.publish_runtime_pane_state_changed(
                        pane_id,
                        *generation,
                        previous,
                        current,
                        Some(reason),
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

    #[cfg(test)]
    pub(crate) fn tick_pane_state_machine_at(&mut self, rt: &tokio::runtime::Handle, now: Instant) {
        self.tick_pane_state_machine_impl(rt, now);
    }

    #[cfg(not(test))]
    fn tick_pane_state_machine_at(&mut self, rt: &tokio::runtime::Handle, now: Instant) {
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
            if let Some((previous, current, reason)) = state_change {
                let generation = self.current_generation_or_default(&pane_id);
                self.publish_runtime_pane_state_changed(
                    &pane_id,
                    generation,
                    previous,
                    current,
                    Some(reason),
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
        if let Some((previous, current, reason)) = state_change {
            self.publish_runtime_pane_state_changed(
                pane_id,
                generation,
                previous,
                current,
                Some(reason),
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
