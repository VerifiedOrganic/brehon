//! Runtime policy gates for mutating operations.
//!
//! Policies are deliberately evaluated before command execution. They do not
//! mutate mux state; they return a decision that callers must audit and honor.

use async_trait::async_trait;
use brehon_ports::{PolicyGate, PortError};
use brehon_types::{
    PromptDeliveryMode, RuntimeCommandKind, RuntimePaneState, RuntimePolicyDecision,
    RuntimePolicyRequest,
};

/// Conservative defaults for local long-running sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePolicyConfig {
    pub max_queued_prompts_per_pane: usize,
    pub max_broadcast_fanout: usize,
    pub max_recent_failures: usize,
    pub default_rate_limit_defer_ms: u64,
}

impl Default for RuntimePolicyConfig {
    fn default() -> Self {
        Self {
            max_queued_prompts_per_pane: 8,
            max_broadcast_fanout: 16,
            max_recent_failures: 3,
            default_rate_limit_defer_ms: 30_000,
        }
    }
}

/// Baseline policy gate for runtime commands.
#[derive(Debug, Clone, Default)]
pub struct BasicPolicyGate {
    config: RuntimePolicyConfig,
}

impl BasicPolicyGate {
    pub fn new(config: RuntimePolicyConfig) -> Self {
        Self { config }
    }

    fn evaluate_sync(&self, request: &RuntimePolicyRequest) -> RuntimePolicyDecision {
        if request.context.approval_required {
            return RuntimePolicyDecision::RequireApproval {
                reason: "operation requires explicit approval".to_string(),
            };
        }

        if let Some(until_ms) = request.context.rate_limited_until_ms
            && until_ms > request.command.issued_at_ms
        {
            return RuntimePolicyDecision::Defer {
                retry_after_ms: until_ms.saturating_sub(request.command.issued_at_ms),
                reason: "pane is in rate-limit cooldown".to_string(),
            };
        }

        if matches!(request.context.pane_state, Some(RuntimePaneState::Dead))
            && command_requires_live_pane(&request.command.kind)
        {
            return RuntimePolicyDecision::Deny {
                reason: "operation requires a live pane".to_string(),
            };
        }
        if matches!(request.context.pane_state, Some(RuntimePaneState::Blocked))
            && command_requires_unblocked_pane(&request.command.kind)
        {
            return RuntimePolicyDecision::Deny {
                reason: "operation requires a pane that is not prompt-blocked".to_string(),
            };
        }

        if let Some(recent_failures) = request.context.recent_failures
            && recent_failures >= self.config.max_recent_failures
        {
            return RuntimePolicyDecision::Deny {
                reason: format!(
                    "recent failure count {recent_failures} reached circuit breaker threshold {}",
                    self.config.max_recent_failures
                ),
            };
        }

        match &request.command.kind {
            RuntimeCommandKind::SendPrompt { delivery, .. } => {
                if matches!(request.context.pane_state, Some(RuntimePaneState::Busy))
                    && *delivery == PromptDeliveryMode::Direct
                {
                    return RuntimePolicyDecision::Deny {
                        reason: "direct prompt delivery to a busy pane is not allowed".to_string(),
                    };
                }
                if let Some(queued_prompts) = request.context.queued_prompts
                    && queued_prompts >= self.config.max_queued_prompts_per_pane
                {
                    return RuntimePolicyDecision::Defer {
                        retry_after_ms: self.config.default_rate_limit_defer_ms,
                        reason: format!(
                            "pane prompt queue is full ({queued_prompts}/{})",
                            self.config.max_queued_prompts_per_pane
                        ),
                    };
                }
                RuntimePolicyDecision::Allow
            }
            RuntimeCommandKind::BroadcastPrompt { pane_ids, .. } => {
                let fanout = request.context.broadcast_fanout.unwrap_or(pane_ids.len());
                if fanout > self.config.max_broadcast_fanout {
                    return RuntimePolicyDecision::Deny {
                        reason: format!(
                            "broadcast fanout {fanout} exceeds limit {}",
                            self.config.max_broadcast_fanout
                        ),
                    };
                }
                RuntimePolicyDecision::Allow
            }
            RuntimeCommandKind::SendTerminalInput { .. }
            | RuntimeCommandKind::Interrupt { .. }
            | RuntimeCommandKind::ResetPane { .. }
            | RuntimeCommandKind::RecyclePane { .. }
            | RuntimeCommandKind::QuarantinePane { .. }
            | RuntimeCommandKind::SpawnPane { .. }
            | RuntimeCommandKind::ResizePane { .. }
            | RuntimeCommandKind::ClosePane { .. }
            | RuntimeCommandKind::ResolveApproval { .. } => RuntimePolicyDecision::Allow,
        }
    }
}

fn command_requires_unblocked_pane(kind: &RuntimeCommandKind) -> bool {
    matches!(
        kind,
        RuntimeCommandKind::SendPrompt { .. }
            | RuntimeCommandKind::SendTerminalInput { .. }
            | RuntimeCommandKind::Interrupt { .. }
    )
}

#[async_trait]
impl PolicyGate for BasicPolicyGate {
    async fn evaluate(
        &self,
        request: RuntimePolicyRequest,
    ) -> Result<RuntimePolicyDecision, PortError> {
        Ok(self.evaluate_sync(&request))
    }

    fn evaluate_immediate(
        &self,
        request: RuntimePolicyRequest,
    ) -> Option<Result<RuntimePolicyDecision, PortError>> {
        Some(Ok(self.evaluate_sync(&request)))
    }
}

fn command_requires_live_pane(kind: &RuntimeCommandKind) -> bool {
    matches!(
        kind,
        RuntimeCommandKind::SendPrompt { .. }
            | RuntimeCommandKind::SendTerminalInput { .. }
            | RuntimeCommandKind::Interrupt { .. }
            | RuntimeCommandKind::ResizePane { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{
        RuntimeCommand, RuntimeCommandTarget, RuntimePaneKind, RuntimePolicyContext,
    };

    fn request(kind: RuntimeCommandKind, context: RuntimePolicyContext) -> RuntimePolicyRequest {
        RuntimePolicyRequest {
            command: RuntimeCommand {
                command_id: "cmd".to_string(),
                target: RuntimeCommandTarget {
                    session_id: "session".to_string(),
                    pane_id: Some("pane".to_string()),
                    generation: Some(1),
                },
                issued_at_ms: 1_000,
                kind,
            },
            context,
        }
    }

    #[tokio::test]
    async fn denies_direct_prompt_to_busy_pane() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::SendPrompt {
                    prompt_id: "p".to_string(),
                    text: "hello".to_string(),
                    from: None,
                    delivery: PromptDeliveryMode::Direct,
                },
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Busy),
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert!(matches!(decision, RuntimePolicyDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn allows_queued_prompt_to_busy_pane() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::SendPrompt {
                    prompt_id: "p".to_string(),
                    text: "hello".to_string(),
                    from: None,
                    delivery: PromptDeliveryMode::Enqueue,
                },
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Busy),
                    queued_prompts: Some(1),
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert_eq!(decision, RuntimePolicyDecision::Allow);
    }

    #[tokio::test]
    async fn defers_when_prompt_queue_is_full() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::SendPrompt {
                    prompt_id: "p".to_string(),
                    text: "hello".to_string(),
                    from: None,
                    delivery: PromptDeliveryMode::Enqueue,
                },
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Ready),
                    queued_prompts: Some(8),
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert!(matches!(decision, RuntimePolicyDecision::Defer { .. }));
    }

    #[tokio::test]
    async fn requires_approval_when_context_requests_it() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::Interrupt {
                    reason: "operator".to_string(),
                },
                RuntimePolicyContext {
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert!(matches!(
            decision,
            RuntimePolicyDecision::RequireApproval { .. }
        ));
    }

    #[tokio::test]
    async fn permission_sensitive_command_requires_explicit_approval() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::SendTerminalInput {
                    bytes: b"rm -rf target".to_vec(),
                },
                RuntimePolicyContext {
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert!(matches!(
            decision,
            RuntimePolicyDecision::RequireApproval { .. }
        ));
    }

    #[tokio::test]
    async fn denies_terminal_input_to_dead_pane() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::SendTerminalInput { bytes: vec![b'x'] },
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Dead),
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert!(matches!(decision, RuntimePolicyDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn denies_direct_prompt_to_blocked_pane() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::SendPrompt {
                    prompt_id: "p".to_string(),
                    text: "resume".to_string(),
                    from: None,
                    delivery: PromptDeliveryMode::Direct,
                },
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Blocked),
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert!(matches!(decision, RuntimePolicyDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn allows_close_for_dead_pane_cleanup() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::ClosePane {
                    reason: "cleanup".to_string(),
                },
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Dead),
                    ..RuntimePolicyContext::default()
                },
            ))
            .await
            .expect("policy");

        assert_eq!(decision, RuntimePolicyDecision::Allow);
    }

    #[tokio::test]
    async fn denies_oversized_broadcast() {
        let gate = BasicPolicyGate::new(RuntimePolicyConfig {
            max_broadcast_fanout: 2,
            ..RuntimePolicyConfig::default()
        });
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::BroadcastPrompt {
                    prompt_id: "p".to_string(),
                    text: "hello".to_string(),
                    pane_ids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                },
                RuntimePolicyContext::default(),
            ))
            .await
            .expect("policy");

        assert!(matches!(decision, RuntimePolicyDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn spawn_is_allowed_even_without_pane_state() {
        let gate = BasicPolicyGate::default();
        let decision = gate
            .evaluate(request(
                RuntimeCommandKind::SpawnPane {
                    kind: RuntimePaneKind::Worker,
                    pane_id: Some("worker".to_string()),
                    title: None,
                    cwd: None,
                    command: Vec::new(),
                    env: std::collections::BTreeMap::new(),
                    rows: None,
                    cols: None,
                },
                RuntimePolicyContext::default(),
            ))
            .await
            .expect("policy");

        assert_eq!(decision, RuntimePolicyDecision::Allow);
    }
}
