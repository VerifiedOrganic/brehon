//! Pane state machine types.
//!
//! This module contains the authoritative pane lifecycle state machine.

use crate::mux::{MAX_TURN_DURATION, PromptQueuePosition, QUIET_THRESHOLD};
use brehon_types::{PromptId, RuntimePaneBlockInfo, RuntimePaneBlockKind};
use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

use super::types::{BlockedResumeState, Pane};

/// Monotonic pane generation identifier.
///
/// A pane increments generations when its backend session is recycled so
/// stale queued prompts or activity can be fenced out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(pub u64);

/// Default max depth for per-pane delayed prompt waiting queue.
pub(crate) const DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP: usize = 8;

/// A delayed prompt queued for eventual delivery to a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedPrompt {
    pub(crate) prompt_id: PromptId,
    pub(crate) prompt: String,
    pub(crate) from: Option<String>,
    pub(crate) inject_after: Instant,
    pub(crate) generation: Generation,
}

/// Per-pane delayed prompt queue.
///
/// Exactly one prompt may be in-flight at a time. Additional prompts wait in a
/// bounded FIFO queue.
#[derive(Debug, Default, Clone)]
pub(crate) struct PanePromptQueue {
    pub(crate) in_flight: Option<QueuedPrompt>,
    pub(crate) waiting: VecDeque<QueuedPrompt>,
}

impl PanePromptQueue {
    pub(crate) fn total_len(&self) -> usize {
        usize::from(self.in_flight.is_some()) + self.waiting.len()
    }

    fn enqueue(&mut self, pane_id: &str, queued: QueuedPrompt, waiting_cap: usize) -> bool {
        if self.in_flight.is_none() {
            self.in_flight = Some(queued);
            return true;
        }

        let attempted_depth = self.waiting.len().saturating_add(1);
        if self.waiting.len() >= waiting_cap {
            tracing::warn!(
                pane_id = %pane_id,
                depth = attempted_depth,
                "queue depth exceeded"
            );
            return false;
        }

        self.waiting.push_back(queued);
        true
    }

    fn promote_waiting_to_in_flight(&mut self, pane_id: &str, pane_generation: Generation) {
        if self.in_flight.is_some() {
            return;
        }

        while let Some(queued) = self.waiting.pop_front() {
            if queued.generation == pane_generation {
                self.in_flight = Some(queued);
                return;
            }

            tracing::info!(
                pane_id = %pane_id,
                prompt_id = %queued.prompt_id,
                prompt_gen = queued.generation.0,
                pane_gen = pane_generation.0,
                "dropped stale queued prompt after recycle"
            );
        }
    }

    fn take_ready_in_flight(&mut self, now: Instant) -> Option<QueuedPrompt> {
        let ready = self
            .in_flight
            .as_ref()
            .is_some_and(|queued| now >= queued.inject_after);
        if ready { self.in_flight.take() } else { None }
    }

    fn find_by_content(
        &self,
        prompt: &str,
        from: Option<&str>,
    ) -> Option<(PromptId, PromptQueuePosition)> {
        if let Some(queued) = self.in_flight.as_ref()
            && queued.prompt == prompt
            && queued.from.as_deref() == from
        {
            return Some((queued.prompt_id.clone(), PromptQueuePosition::InFlight));
        }

        self.waiting
            .iter()
            .enumerate()
            .find(|(_idx, queued)| queued.prompt == prompt && queued.from.as_deref() == from)
            .map(|(idx, queued)| (queued.prompt_id.clone(), PromptQueuePosition::Waiting(idx)))
    }

    fn find_by_prompt_id(&self, prompt_id: &PromptId) -> Option<PromptQueuePosition> {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|queued| queued.prompt_id == *prompt_id)
        {
            return Some(PromptQueuePosition::InFlight);
        }

        self.waiting
            .iter()
            .enumerate()
            .find(|(_idx, queued)| queued.prompt_id == *prompt_id)
            .map(|(idx, _queued)| PromptQueuePosition::Waiting(idx))
    }
}

/// Why a pane has entered a terminal dead state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeathReason {
    /// Supervisor-authoritative quarantine with operator context.
    Quarantined(String),
    /// The pane remained busy beyond the maximum turn duration.
    MaxTurnExceeded,
    /// The backend session dropped unexpectedly.
    SessionDropped,
    /// The backend failed to initialize or restart.
    SpawnFailed(String),
    /// Transport closed unexpectedly while the pane was active.
    TransportClosed,
}

/// Explicit per-pane lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneState {
    /// Pane is available for immediate prompt delivery.
    Ready { since: Instant },
    /// Pane currently has a prompt in flight.
    Busy {
        prompt_id: PromptId,
        generation: Generation,
        delivered_at: Instant,
        last_activity_at: Instant,
    },
    /// Pane is blocked on a prompt Brehon cannot answer automatically.
    Blocked {
        info: RuntimePaneBlockInfo,
        at: Instant,
    },
    /// Pane is terminally dead until replaced.
    Dead { reason: DeathReason, at: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyReadyFastPath {
    TurnEnded,
    OperationCompleted,
}

impl BusyReadyFastPath {
    fn as_str(self) -> &'static str {
        match self {
            Self::TurnEnded => "turnEnded",
            Self::OperationCompleted => "OperationCompleted",
        }
    }
}

impl Pane {
    const PERMISSION_RESOLUTION_FALLBACK_WINDOW: Duration = Duration::from_secs(30);

    fn blocked_permission_resolution_fallback_pending(info: &RuntimePaneBlockInfo) -> bool {
        matches!(info.kind, RuntimePaneBlockKind::TerminalPrompt)
            || matches!(
                info.kind,
                RuntimePaneBlockKind::PermissionRequest if info.request_id.is_none()
            )
    }

    fn blocked_permission_resolution_fallback_until(
        info: &RuntimePaneBlockInfo,
        now: Instant,
    ) -> Option<Instant> {
        Self::blocked_permission_resolution_fallback_pending(info)
            .then_some(now + Self::PERMISSION_RESOLUTION_FALLBACK_WINDOW)
    }

    fn clear_blocked_resume_state(&mut self) {
        self.blocked_resume_state = None;
    }

    fn capture_blocked_resume_state(&mut self) {
        if self.blocked_resume_state.is_some() {
            return;
        }
        self.blocked_resume_state = Some(BlockedResumeState {
            pane_state: self.pane_state.clone(),
            tool_executing: self.is_tool_executing,
        });
    }

    pub(crate) fn delayed_prompt_count(&self) -> usize {
        self.prompt_queue.total_len()
    }

    #[cfg(test)]
    pub(crate) fn delayed_prompt_in_flight(&self) -> Option<&QueuedPrompt> {
        self.prompt_queue.in_flight.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn delayed_prompt_waiting(&self) -> &VecDeque<QueuedPrompt> {
        &self.prompt_queue.waiting
    }

    #[cfg(test)]
    pub(crate) fn delayed_prompt_in_flight_mut(&mut self) -> Option<&mut QueuedPrompt> {
        self.prompt_queue.in_flight.as_mut()
    }

    pub(crate) fn enqueue_delayed_prompt(
        &mut self,
        queued: QueuedPrompt,
        waiting_cap: usize,
    ) -> bool {
        self.prompt_queue.enqueue(&self.id, queued, waiting_cap)
    }

    pub(crate) fn delayed_prompt_position_by_content(
        &self,
        prompt: &str,
        from: Option<&str>,
    ) -> Option<(PromptId, PromptQueuePosition)> {
        self.prompt_queue.find_by_content(prompt, from)
    }

    pub(crate) fn delayed_prompt_position_by_id(
        &self,
        prompt_id: &PromptId,
    ) -> Option<PromptQueuePosition> {
        self.prompt_queue.find_by_prompt_id(prompt_id)
    }

    pub(crate) fn take_ready_delayed_prompt(&mut self, now: Instant) -> Option<QueuedPrompt> {
        self.prompt_queue.take_ready_in_flight(now)
    }

    pub(crate) fn promote_waiting_delayed_prompt(&mut self) {
        self.prompt_queue
            .promote_waiting_to_in_flight(&self.id, self.current_generation());
    }

    /// Advance the pane state machine by one tick.
    ///
    /// Busy panes become ready when:
    /// - quiet for `QUIET_THRESHOLD`, or
    /// - force-closed after `MAX_TURN_DURATION` (with a warning).
    pub fn tick_state_machine(&mut self, now: Instant) -> bool {
        let next = match self.pane_state.as_ref() {
            Some(PaneState::Busy {
                prompt_id,
                delivered_at,
                last_activity_at,
                ..
            }) => {
                if now.saturating_duration_since(*last_activity_at) >= QUIET_THRESHOLD {
                    Some(PaneState::Ready { since: now })
                } else if now.saturating_duration_since(*delivered_at) >= MAX_TURN_DURATION {
                    tracing::warn!(
                        pane_id = %self.id,
                        prompt_id = %prompt_id,
                        max_turn_duration_ms = %MAX_TURN_DURATION.as_millis(),
                        delivered_for_ms = %now.saturating_duration_since(*delivered_at).as_millis(),
                        "Pane state machine forced Busy → Ready after max turn duration"
                    );
                    Some(PaneState::Ready { since: now })
                } else {
                    None
                }
            }
            Some(PaneState::Blocked { .. }) => None,
            _ => None,
        };

        if let Some(next) = next {
            self.pane_state = Some(next);
            return true;
        }

        false
    }

    /// Fast-path Busy → Ready transition for explicit `turnEnded` events.
    pub fn state_machine_turn_ended(&mut self, generation: Generation, now: Instant) -> bool {
        self.transition_busy_to_ready_fast_path(generation, now, BusyReadyFastPath::TurnEnded)
    }

    /// Fast-path Busy → Ready transition for explicit `OperationCompleted` events.
    pub fn state_machine_operation_completed(
        &mut self,
        generation: Generation,
        now: Instant,
    ) -> bool {
        self.transition_busy_to_ready_fast_path(
            generation,
            now,
            BusyReadyFastPath::OperationCompleted,
        )
    }

    /// Current pane state, if present.
    pub fn pane_state(&self) -> Option<&PaneState> {
        self.pane_state.as_ref()
    }

    /// Store the latest pane state without applying Ready/Busy dead-state guards.
    ///
    /// This is intended for lifecycle transitions that must be able to write
    /// `Dead` or reset a pane directly during recycle/quarantine flows.
    /// Prefer `set_pane_ready()` and `set_pane_busy()` for ordinary activity
    /// updates so dead panes cannot be revived accidentally.
    pub(crate) fn set_pane_state(&mut self, state: PaneState) {
        if matches!(self.pane_state.as_ref(), Some(PaneState::Blocked { .. }))
            && !matches!(state, PaneState::Blocked { .. })
        {
            self.terminal_prompt_prefilter_tail.clear();
            self.permission_resolution_fallback_until = None;
        }
        if !matches!(state, PaneState::Blocked { .. }) {
            self.clear_blocked_resume_state();
        }
        self.pane_state = Some(state);
    }

    pub(crate) fn set_pane_ready(&mut self, now: Instant) {
        if matches!(
            self.pane_state.as_ref(),
            Some(PaneState::Dead { .. } | PaneState::Blocked { .. })
        ) {
            return;
        }
        self.clear_blocked_resume_state();
        self.permission_resolution_fallback_until = None;
        self.pane_state = Some(PaneState::Ready { since: now });
    }

    fn begin_external_ready_or_busy_transition(&mut self) -> bool {
        if matches!(self.pane_state.as_ref(), Some(PaneState::Dead { .. })) {
            return false;
        }
        if matches!(self.pane_state.as_ref(), Some(PaneState::Blocked { .. })) {
            self.terminal_prompt_prefilter_tail.clear();
        }
        self.clear_blocked_resume_state();
        self.permission_resolution_fallback_until = None;
        true
    }

    pub(crate) fn set_external_pane_ready(&mut self, now: Instant) {
        if !self.begin_external_ready_or_busy_transition() {
            return;
        }
        self.pane_state = Some(PaneState::Ready { since: now });
    }

    pub(crate) fn set_pane_busy(
        &mut self,
        prompt_id: PromptId,
        generation: Generation,
        now: Instant,
    ) {
        if matches!(
            self.pane_state.as_ref(),
            Some(PaneState::Dead { .. } | PaneState::Blocked { .. })
        ) {
            return;
        }
        self.clear_blocked_resume_state();
        self.permission_resolution_fallback_until = None;
        self.pane_state = Some(PaneState::Busy {
            prompt_id,
            generation,
            delivered_at: now,
            last_activity_at: now,
        });
    }

    pub(crate) fn set_external_pane_busy(
        &mut self,
        prompt_id: PromptId,
        generation: Generation,
        now: Instant,
    ) {
        if !self.begin_external_ready_or_busy_transition() {
            return;
        }
        self.pane_state = Some(PaneState::Busy {
            prompt_id,
            generation,
            delivered_at: now,
            last_activity_at: now,
        });
    }

    pub(crate) fn set_pane_blocked(&mut self, info: RuntimePaneBlockInfo, now: Instant) {
        if matches!(self.pane_state.as_ref(), Some(PaneState::Dead { .. })) {
            return;
        }
        if !matches!(self.pane_state.as_ref(), Some(PaneState::Blocked { .. })) {
            self.capture_blocked_resume_state();
        }
        if let Some(PaneState::Blocked { info: existing, .. }) = self.pane_state.as_ref() {
            tracing::warn!(
                pane_id = %self.id,
                previous = ?existing,
                replacement = ?info,
                "Overwriting existing blocked pane state"
            );
        }
        self.permission_resolution_fallback_until =
            Self::blocked_permission_resolution_fallback_until(&info, now);
        self.pane_state = Some(PaneState::Blocked { info, at: now });
    }

    pub(crate) fn refresh_blocked_info(&mut self, info: RuntimePaneBlockInfo, now: Instant) {
        if matches!(self.pane_state.as_ref(), Some(PaneState::Blocked { .. })) {
            self.permission_resolution_fallback_until =
                Self::blocked_permission_resolution_fallback_until(&info, now);
            self.pane_state = Some(PaneState::Blocked { info, at: now });
        }
    }

    pub(crate) fn restore_after_blocked_permission_resolution(
        &mut self,
        fallback_prompt_id: PromptId,
        fallback_generation: Generation,
        now: Instant,
    ) {
        let resume = self.blocked_resume_state.take();
        self.terminal_prompt_prefilter_tail.clear();
        let tool_executing = resume.as_ref().map_or(true, |snapshot| {
            snapshot.tool_executing || matches!(snapshot.pane_state, Some(PaneState::Busy { .. }))
        });
        let (prompt_id, generation) = match resume.as_ref() {
            Some(BlockedResumeState {
                pane_state:
                    Some(PaneState::Busy {
                        prompt_id,
                        generation,
                        ..
                    }),
                ..
            }) => (prompt_id.clone(), *generation),
            Some(BlockedResumeState { .. }) | None => (fallback_prompt_id, fallback_generation),
        };
        self.is_tool_executing = tool_executing;
        self.permission_resolution_fallback_until = None;
        self.pane_state = Some(PaneState::Busy {
            prompt_id,
            generation,
            delivered_at: now,
            last_activity_at: now,
        });
    }

    pub(crate) fn clear_permission_resolution_fallback(&mut self) {
        self.permission_resolution_fallback_until = None;
    }

    pub(crate) fn permission_resolution_fallback_pending(&self, now: Instant) -> bool {
        self.permission_resolution_fallback_until
            .is_some_and(|deadline| now <= deadline)
    }

    pub(crate) fn permission_resolution_fallback_expired(&self, now: Instant) -> bool {
        self.permission_resolution_fallback_until
            .is_some_and(|deadline| now > deadline)
    }

    #[cfg(test)]
    pub(crate) fn expire_permission_resolution_fallback_for_test(&mut self) {
        self.permission_resolution_fallback_until = Some(Instant::now() - Duration::from_secs(1));
    }

    pub(crate) fn touch_busy_activity(&mut self, now: Instant) {
        if let Some(PaneState::Busy {
            last_activity_at, ..
        }) = self.pane_state.as_mut()
        {
            *last_activity_at = now;
        }
    }

    fn transition_busy_to_ready_fast_path(
        &mut self,
        generation: Generation,
        now: Instant,
        fast_path: BusyReadyFastPath,
    ) -> bool {
        let Some(PaneState::Busy {
            generation: busy_generation,
            ..
        }) = self.pane_state.as_ref()
        else {
            return false;
        };

        if *busy_generation != generation {
            return false;
        }

        tracing::debug!(
            pane_id = %self.id,
            generation = generation.0,
            fast_path = fast_path.as_str(),
            "Pane state machine fast-path Busy → Ready"
        );
        self.pane_state = Some(PaneState::Ready { since: now });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_pane() -> Pane {
        Pane::director("state-machine-test", 24, 80).expect("create director pane")
    }

    fn busy_state(
        prompt_id: &str,
        generation: Generation,
        delivered_at: Instant,
        last_activity_at: Instant,
    ) -> PaneState {
        PaneState::Busy {
            prompt_id: PromptId::new(prompt_id.to_string()),
            generation,
            delivered_at,
            last_activity_at,
        }
    }

    #[test]
    fn busy_transitions_to_ready_on_quiet_threshold() {
        let now = Instant::now();
        let mut pane = make_test_pane();
        pane.set_pane_state(busy_state(
            "quiet-threshold",
            Generation(7),
            now - Duration::from_secs(5),
            now - QUIET_THRESHOLD,
        ));

        assert!(pane.tick_state_machine(now));
        assert!(matches!(
            pane.pane_state(),
            Some(PaneState::Ready { since }) if *since == now
        ));
    }

    #[test]
    fn busy_transitions_to_ready_on_turn_ended_fast_path() {
        let now = Instant::now();
        let mut pane = make_test_pane();
        pane.set_pane_state(busy_state(
            "turn-ended",
            Generation(11),
            now - Duration::from_secs(2),
            now - Duration::from_secs(1),
        ));

        assert!(!pane.state_machine_turn_ended(Generation(10), now));
        assert!(pane.state_machine_turn_ended(Generation(11), now));
        assert!(matches!(
            pane.pane_state(),
            Some(PaneState::Ready { since }) if *since == now
        ));
    }

    #[test]
    fn busy_transitions_to_ready_on_operation_completed_fast_path() {
        let now = Instant::now();
        let mut pane = make_test_pane();
        pane.set_pane_state(busy_state(
            "op-completed",
            Generation(4),
            now - Duration::from_secs(2),
            now - Duration::from_secs(1),
        ));

        assert!(!pane.state_machine_operation_completed(Generation(5), now));
        assert!(pane.state_machine_operation_completed(Generation(4), now));
        assert!(matches!(
            pane.pane_state(),
            Some(PaneState::Ready { since }) if *since == now
        ));
    }

    #[test]
    fn busy_transitions_to_ready_after_max_turn_duration() {
        let now = Instant::now();
        let mut pane = make_test_pane();
        pane.set_pane_state(busy_state(
            "max-turn",
            Generation(9),
            now - MAX_TURN_DURATION,
            now,
        ));

        assert!(pane.tick_state_machine(now));
        assert!(matches!(
            pane.pane_state(),
            Some(PaneState::Ready { since }) if *since == now
        ));
    }

    #[test]
    fn set_pane_ready_preserves_dead_state() {
        let now = Instant::now();
        let mut pane = make_test_pane();
        let reason = DeathReason::Quarantined("manual quarantine".to_string());
        pane.set_pane_state(PaneState::Dead {
            reason: reason.clone(),
            at: now,
        });
        pane.set_pane_ready(now + Duration::from_secs(1));
        assert_eq!(
            pane.pane_state(),
            Some(&PaneState::Dead {
                reason: reason.clone(),
                at: now
            })
        );

        pane.set_pane_busy(PromptId::new("dead-busy".to_string()), Generation(1), now);
        assert_eq!(
            pane.pane_state(),
            Some(&PaneState::Dead { reason, at: now })
        );
    }

    #[test]
    fn external_ready_and_busy_recovery_allow_blocked_but_preserve_dead() {
        let now = Instant::now();
        let blocked = RuntimePaneBlockInfo {
            kind: RuntimePaneBlockKind::PermissionRequest,
            summary: "permission request blocked automatic recovery".to_string(),
            command_or_tool: Some("allow bash ls".to_string()),
            request_id: Some("perm-1".to_string()),
            task_id: Some("T-1".to_string()),
            excerpt: Some("Permission request: allow bash ls".to_string()),
        };

        let mut blocked_ready = make_test_pane();
        blocked_ready.set_pane_blocked(blocked.clone(), now);
        blocked_ready.set_external_pane_ready(now + Duration::from_secs(1));
        assert!(matches!(
            blocked_ready.pane_state(),
            Some(PaneState::Ready { .. })
        ));

        let mut blocked_busy = make_test_pane();
        blocked_busy.set_pane_blocked(blocked, now);
        blocked_busy.set_external_pane_busy(
            PromptId::new("blocked-busy".to_string()),
            Generation(2),
            now + Duration::from_secs(1),
        );
        assert!(matches!(
            blocked_busy.pane_state(),
            Some(PaneState::Busy { generation, .. }) if *generation == Generation(2)
        ));

        let mut dead = make_test_pane();
        let reason = DeathReason::SessionDropped;
        dead.set_pane_state(PaneState::Dead { reason, at: now });
        dead.set_external_pane_ready(now + Duration::from_secs(1));
        assert!(matches!(dead.pane_state(), Some(PaneState::Dead { .. })));
        dead.set_external_pane_busy(
            PromptId::new("dead-busy".to_string()),
            Generation(3),
            now + Duration::from_secs(2),
        );
        assert!(matches!(dead.pane_state(), Some(PaneState::Dead { .. })));
    }

    #[test]
    fn provider_context_limit_block_does_not_arm_permission_resolution_fallback() {
        let now = Instant::now();
        let mut pane = make_test_pane();
        pane.set_pane_blocked(
            RuntimePaneBlockInfo {
                kind: RuntimePaneBlockKind::ProviderContextLimit,
                summary: "provider context limit blocked automatic recovery".to_string(),
                command_or_tool: Some("context window exceeds limit".to_string()),
                request_id: Some("busy-prompt".to_string()),
                task_id: Some("T-1".to_string()),
                excerpt: Some("API Error: context window exceeds limit".to_string()),
            },
            now,
        );

        assert!(!pane.permission_resolution_fallback_pending(now));
    }
}
