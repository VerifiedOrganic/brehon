//! Runtime policy helpers for mux mutation paths.

use std::sync::Arc;

use brehon_ports::PolicyGate;
use brehon_types::{
    PolicyDecisionEvent, RuntimeCommand, RuntimeCommandKind, RuntimeCommandTarget, RuntimeEvent,
    RuntimeEventKind, RuntimeEventMeta, RuntimeOperation, RuntimePaneState, RuntimePolicyContext,
    RuntimePolicyDecision, RuntimePolicyRequest, RuntimeSource,
};

use super::Mux;
use super::runtime::unix_timestamp_ms;
use crate::error::Error;
use crate::pane::{DeathReason, Generation, PaneState};

impl Mux {
    /// Install a policy gate for mutating runtime operations.
    pub fn set_policy_gate(&mut self, gate: Arc<dyn PolicyGate>) {
        self.policy_gate = Some(gate);
    }

    /// Disable runtime policy checks.
    pub fn clear_policy_gate(&mut self) {
        self.policy_gate = None;
    }

    pub(crate) fn runtime_command_for_pane(
        &self,
        pane_id: &str,
        kind: RuntimeCommandKind,
    ) -> RuntimeCommand {
        RuntimeCommand {
            command_id: format!("cmd-{}", uuid::Uuid::new_v4()),
            target: RuntimeCommandTarget {
                session_id: self
                    .session_name
                    .as_deref()
                    .unwrap_or("default")
                    .to_string(),
                pane_id: Some(pane_id.to_string()),
                generation: self
                    .panes
                    .get(pane_id)
                    .map(|pane| pane.current_generation().0),
            },
            issued_at_ms: unix_timestamp_ms(),
            kind,
        }
    }

    pub(crate) fn runtime_command_for_session(&self, kind: RuntimeCommandKind) -> RuntimeCommand {
        RuntimeCommand {
            command_id: format!("cmd-{}", uuid::Uuid::new_v4()),
            target: RuntimeCommandTarget {
                session_id: self
                    .session_name
                    .as_deref()
                    .unwrap_or("default")
                    .to_string(),
                pane_id: None,
                generation: None,
            },
            issued_at_ms: unix_timestamp_ms(),
            kind,
        }
    }

    pub(crate) fn runtime_policy_context_for_pane(&self, pane_id: &str) -> RuntimePolicyContext {
        let pane = self.panes.get(pane_id);
        RuntimePolicyContext {
            pane_state: pane
                .and_then(|pane| pane.pane_state())
                .map(runtime_pane_state)
                .or(Some(RuntimePaneState::Unknown)),
            queued_prompts: pane.map(|pane| pane.delayed_prompt_count()),
            broadcast_fanout: None,
            recent_failures: None,
            rate_limited_until_ms: None,
            approval_required: false,
        }
    }

    pub(crate) fn runtime_policy_context_for_broadcast(
        &self,
        fanout: usize,
    ) -> RuntimePolicyContext {
        RuntimePolicyContext {
            broadcast_fanout: Some(fanout),
            ..RuntimePolicyContext::default()
        }
    }

    pub(crate) async fn evaluate_runtime_policy(
        &self,
        command: RuntimeCommand,
        context: RuntimePolicyContext,
    ) -> RuntimePolicyDecision {
        let Some(gate) = self.policy_gate.clone() else {
            return RuntimePolicyDecision::Allow;
        };

        let request = RuntimePolicyRequest {
            command: command.clone(),
            context,
        };
        let decision = match gate.evaluate(request).await {
            Ok(decision) => decision,
            Err(err) => RuntimePolicyDecision::Deny {
                reason: format!("policy evaluation failed: {err}"),
            },
        };
        self.publish_policy_decision(&command, &decision).await;
        decision
    }

    pub(crate) fn evaluate_runtime_policy_immediate(
        &self,
        command: RuntimeCommand,
        context: RuntimePolicyContext,
    ) -> RuntimePolicyDecision {
        let Some(gate) = self.policy_gate.clone() else {
            return RuntimePolicyDecision::Allow;
        };

        let request = RuntimePolicyRequest {
            command: command.clone(),
            context,
        };
        let decision = match gate.evaluate_immediate(request) {
            Some(Ok(decision)) => decision,
            Some(Err(err)) => RuntimePolicyDecision::Deny {
                reason: format!("policy evaluation failed: {err}"),
            },
            None => RuntimePolicyDecision::Deny {
                reason: "policy gate cannot evaluate from a synchronous mux path".to_string(),
            },
        };
        self.publish_policy_decision_nonblocking(&command, &decision);
        decision
    }

    pub(crate) fn policy_decision_error(
        operation: &str,
        decision: &RuntimePolicyDecision,
    ) -> Option<Error> {
        match decision {
            RuntimePolicyDecision::Allow => None,
            RuntimePolicyDecision::Deny { reason } => {
                Some(Error::pty(format!("Policy denied {operation}: {reason}")))
            }
            RuntimePolicyDecision::Defer {
                retry_after_ms,
                reason,
            } => Some(Error::pty(format!(
                "Policy deferred {operation} for {retry_after_ms}ms: {reason}"
            ))),
            RuntimePolicyDecision::RequireApproval { reason } => Some(Error::pty(format!(
                "Policy requires approval for {operation}: {reason}"
            ))),
        }
    }

    pub(crate) fn policy_rejection_reason(decision: &RuntimePolicyDecision) -> DeathReason {
        match decision {
            RuntimePolicyDecision::Allow => {
                DeathReason::Quarantined("policy unexpectedly rejected an allowed command".into())
            }
            RuntimePolicyDecision::Deny { reason } => {
                DeathReason::Quarantined(format!("policy denied prompt delivery: {reason}"))
            }
            RuntimePolicyDecision::Defer { reason, .. } => {
                DeathReason::Quarantined(format!("policy deferred prompt delivery: {reason}"))
            }
            RuntimePolicyDecision::RequireApproval { reason } => DeathReason::Quarantined(format!(
                "policy requires approval for prompt delivery: {reason}"
            )),
        }
    }

    async fn publish_policy_decision(
        &self,
        command: &RuntimeCommand,
        decision: &RuntimePolicyDecision,
    ) {
        let Some(sink) = self.runtime_event_sink.clone() else {
            return;
        };
        let event = self.policy_decision_event(command, decision);
        if let Err(err) = sink.publish(event).await {
            tracing::warn!(error = %err, "Failed to publish runtime policy decision event");
        }
    }

    fn publish_policy_decision_nonblocking(
        &self,
        command: &RuntimeCommand,
        decision: &RuntimePolicyDecision,
    ) {
        let Some(sink) = self.runtime_event_sink.clone() else {
            return;
        };
        let event = self.policy_decision_event(command, decision);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(err) = sink.publish(event).await {
                        tracing::warn!(
                            error = %err,
                            "Failed to publish runtime policy decision event"
                        );
                    }
                });
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Policy decision event could not be published without an active Tokio runtime"
                );
            }
        }
    }

    fn policy_decision_event(
        &self,
        command: &RuntimeCommand,
        decision: &RuntimePolicyDecision,
    ) -> RuntimeEvent {
        let pane_id = command.target.pane_id.as_deref().unwrap_or("runtime");
        let generation = command
            .target
            .generation
            .map(Generation)
            .unwrap_or_else(|| self.current_generation_or_default(pane_id));
        let meta = RuntimeEventMeta::new(
            command.target.session_id.clone(),
            pane_id.to_string(),
            generation.0,
            RuntimeSource::Policy,
            unix_timestamp_ms(),
        )
        .with_correlation_id(command.command_id.clone());
        RuntimeEvent::new(
            meta,
            RuntimeEventKind::PolicyDecision(PolicyDecisionEvent {
                decision_id: format!("decision-{}", uuid::Uuid::new_v4()),
                operation: runtime_operation_for_command_kind(&command.kind),
                decision: decision.clone(),
            }),
        )
    }
}

fn runtime_pane_state(state: &PaneState) -> RuntimePaneState {
    match state {
        PaneState::Ready { .. } => RuntimePaneState::Ready,
        PaneState::Busy { .. } => RuntimePaneState::Busy,
        PaneState::Blocked { .. } => RuntimePaneState::Blocked,
        PaneState::Dead { .. } => RuntimePaneState::Dead,
    }
}

fn runtime_operation_for_command_kind(kind: &RuntimeCommandKind) -> RuntimeOperation {
    match kind {
        RuntimeCommandKind::SendPrompt { .. } => RuntimeOperation::SendPrompt,
        RuntimeCommandKind::BroadcastPrompt { .. } => RuntimeOperation::BroadcastPrompt,
        RuntimeCommandKind::SendTerminalInput { .. } => RuntimeOperation::SendTerminalInput,
        RuntimeCommandKind::Interrupt { .. } => RuntimeOperation::Interrupt,
        RuntimeCommandKind::ResetPane { .. } => RuntimeOperation::ResetPane,
        RuntimeCommandKind::RecyclePane { .. } => RuntimeOperation::RecyclePane,
        RuntimeCommandKind::QuarantinePane { .. } => RuntimeOperation::QuarantinePane,
        RuntimeCommandKind::SpawnPane { .. } => RuntimeOperation::SpawnPane,
        RuntimeCommandKind::ResizePane { .. } => RuntimeOperation::ResizePane,
        RuntimeCommandKind::ClosePane { .. } => RuntimeOperation::ClosePane,
        RuntimeCommandKind::ResolveApproval { .. } => RuntimeOperation::ResolveApproval,
    }
}
