use crate::pane::{
    DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP, DeathReason, Generation, Pane, PaneState, QueuedPrompt,
};
use brehon_types::PromptId;
use std::time::Instant;

use super::super::Mux;
use super::super::types::PromptDeliveryAttempt;

pub(super) fn startup_prompt_target_generation(pane: Option<&Pane>) -> Generation {
    let Some(pane) = pane else {
        return Generation::default();
    };
    let generation = pane.current_generation();
    if pane.is_gateway_backed() && pane.gateway_session_id().is_none() {
        return Generation(generation.0.saturating_add(1));
    }
    generation
}

impl Mux {
    pub(in crate::mux) fn queue_delayed_prompt(
        &mut self,
        pane_id: &str,
        prompt: String,
        from: Option<String>,
        inject_after: Instant,
        prompt_id: Option<PromptId>,
    ) -> PromptDeliveryAttempt {
        let generation = self.current_generation_or_default(pane_id);
        self.queue_delayed_prompt_for_generation(
            pane_id,
            prompt,
            from,
            inject_after,
            prompt_id,
            generation,
        )
    }

    pub(in crate::mux) fn queue_delayed_prompt_for_generation(
        &mut self,
        pane_id: &str,
        prompt: String,
        from: Option<String>,
        inject_after: Instant,
        prompt_id: Option<PromptId>,
        generation: Generation,
    ) -> PromptDeliveryAttempt {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return PromptDeliveryAttempt::Rejected {
                reason: DeathReason::SessionDropped,
            };
        };

        if let Some(PaneState::Dead { reason, .. }) = pane.pane_state() {
            return PromptDeliveryAttempt::Rejected {
                reason: reason.clone(),
            };
        }

        if let Some(prompt_id) = prompt_id.as_ref()
            && let Some(position) = pane.delayed_prompt_position_by_id(prompt_id)
        {
            return PromptDeliveryAttempt::AlreadyPresent {
                prompt_id: prompt_id.clone(),
                position,
            };
        }

        if let Some((existing_prompt_id, position)) =
            pane.delayed_prompt_position_by_content(&prompt, from.as_deref())
        {
            return PromptDeliveryAttempt::AlreadyPresent {
                prompt_id: existing_prompt_id,
                position,
            };
        }

        let ahead_of = pane.delayed_prompt_count();
        let prompt_id = prompt_id.unwrap_or_else(Self::new_prompt_id);
        let queued = QueuedPrompt {
            prompt_id: prompt_id.clone(),
            prompt,
            from,
            inject_after,
            generation,
        };
        if pane.enqueue_delayed_prompt(queued, DEFAULT_PANE_PROMPT_QUEUE_WAITING_CAP) {
            PromptDeliveryAttempt::Queued {
                prompt_id,
                ahead_of,
            }
        } else {
            tracing::warn!(
                pane = %pane_id,
                generation = generation.0,
                ahead_of,
                "Dropped delayed prompt because per-pane queue depth was exceeded"
            );
            PromptDeliveryAttempt::Rejected {
                reason: DeathReason::Quarantined("prompt queue depth exceeded".to_string()),
            }
        }
    }
}
