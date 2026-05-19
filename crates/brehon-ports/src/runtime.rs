//! Runtime side-channel ports.
//!
//! These traits define the boundary between the mux, future daemon, semantic
//! detectors, policy gates, workflows, and terminal-host adapters.

use brehon_types::{
    RuntimeCommand, RuntimeCommandResult, RuntimeEvent, RuntimePolicyContext,
    RuntimePolicyDecision, RuntimePolicyRequest, TerminalHostCapabilities, TerminalPaneHandle,
    TerminalPaneSpawnSpec, TerminalResize,
};
use async_trait::async_trait;

use crate::PortError;

/// Publishes runtime events without changing mux behavior.
#[async_trait]
pub trait RuntimeEventSink: Send + Sync {
    /// Publish one runtime event.
    async fn publish(&self, event: RuntimeEvent) -> Result<(), PortError>;

    /// Publish a batch of runtime events in order.
    async fn publish_batch(&self, events: Vec<RuntimeEvent>) -> Result<(), PortError> {
        for event in events {
            self.publish(event).await?;
        }
        Ok(())
    }
}

/// Reads runtime events from a side channel.
#[async_trait]
pub trait RuntimeEventStream: Send + Sync {
    /// Return the next event, or `None` when the stream is closed.
    async fn next_event(&mut self) -> Result<Option<RuntimeEvent>, PortError>;
}

/// Executes mutating runtime commands.
#[async_trait]
pub trait RuntimeCommandPort: Send + Sync {
    /// Execute a command after the caller has applied policy.
    async fn execute(&self, command: RuntimeCommand) -> Result<RuntimeCommandResult, PortError>;
}

/// Routes mutating runtime commands through policy and the active command port.
#[async_trait]
pub trait RuntimeCommandRouter: Send + Sync {
    /// Route one command through policy before any execution.
    async fn route_command(
        &self,
        command: RuntimeCommand,
        context: RuntimePolicyContext,
    ) -> Result<RuntimeCommandResult, PortError>;
}

/// Concrete terminal host boundary used by embedded, external, web, or
/// headless hosts.
#[async_trait]
pub trait TerminalHostAdapter: Send + Sync {
    /// Return stable capabilities for this host implementation.
    fn capabilities(&self) -> TerminalHostCapabilities;

    /// Spawn one pane and return the stable runtime handle assigned to it.
    async fn spawn_pane(
        &self,
        spec: TerminalPaneSpawnSpec,
    ) -> Result<TerminalPaneHandle, PortError>;

    /// Resolve the latest live handle for a pane already known to this host.
    async fn pane_handle(
        &self,
        session_id: &str,
        pane_id: &str,
    ) -> Result<TerminalPaneHandle, PortError>;

    /// Return the spawn spec used to create a pane so lifecycle commands can
    /// recreate it without knowing host-specific process state.
    async fn pane_spawn_spec(
        &self,
        session_id: &str,
        pane_id: &str,
    ) -> Result<TerminalPaneSpawnSpec, PortError>;

    /// Close one pane.
    async fn close_pane(&self, handle: TerminalPaneHandle) -> Result<(), PortError>;

    /// Send raw terminal input bytes to one pane.
    async fn send_input(&self, handle: TerminalPaneHandle, bytes: Vec<u8>)
        -> Result<(), PortError>;

    /// Resize one pane.
    async fn resize_pane(
        &self,
        handle: TerminalPaneHandle,
        size: TerminalResize,
    ) -> Result<(), PortError>;
}

/// Polls host-owned terminal state and converts observations into runtime
/// events.
#[async_trait]
pub trait TerminalHostEventObserver: Send + Sync {
    /// Return newly observed host events in stable per-pane order.
    async fn observe_events(&self) -> Result<Vec<RuntimeEvent>, PortError>;
}

/// Policy gate for mutating runtime commands.
#[async_trait]
pub trait PolicyGate: Send + Sync {
    /// Evaluate whether a runtime command is allowed.
    async fn evaluate(
        &self,
        request: RuntimePolicyRequest,
    ) -> Result<RuntimePolicyDecision, PortError>;

    /// Optionally evaluate a command without blocking on an async executor.
    ///
    /// Synchronous mux mutation paths use this hook when a policy gate is
    /// installed but no runtime handle is available at the call site. Gates
    /// that cannot answer immediately should return `None`; callers must then
    /// fail closed rather than bypass policy.
    fn evaluate_immediate(
        &self,
        _request: RuntimePolicyRequest,
    ) -> Option<Result<RuntimePolicyDecision, PortError>> {
        None
    }
}

/// Advisory semantic detector over runtime events.
#[async_trait]
pub trait DetectionEngine: Send + Sync {
    /// Observe one event and return any derived advisory detection events.
    async fn observe(&self, event: RuntimeEvent) -> Result<Vec<RuntimeEvent>, PortError>;
}

/// Sink implementation for tests and launch modes that do not enable runtime
/// side-channel publication yet.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopRuntimeEventSink;

#[async_trait]
impl RuntimeEventSink for NoopRuntimeEventSink {
    async fn publish(&self, _event: RuntimeEvent) -> Result<(), PortError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use brehon_types::{
        RuntimeCommandKind, RuntimeCommandResult, RuntimeCommandStatus, RuntimeCommandTarget,
        RuntimeEventKind, RuntimeEventMeta, RuntimePaneKind, RuntimePolicyContext,
        RuntimePolicyDecision, RuntimePolicyRequest, RuntimeSource,
    };

    fn event() -> RuntimeEvent {
        RuntimeEvent::new(
            RuntimeEventMeta::new("session", "pane", 1, RuntimeSource::Mux, 1),
            RuntimeEventKind::AgentTurnStarted(brehon_types::AgentTurnEvent {
                prompt_id: Some("prompt".to_string()),
                reason: None,
            }),
        )
    }

    #[tokio::test]
    async fn noop_sink_accepts_events() {
        let sink = NoopRuntimeEventSink;
        sink.publish(event()).await.expect("publish event");
        sink.publish_batch(vec![event(), event()])
            .await
            .expect("publish batch");
    }

    struct AllowAllPolicy;

    #[async_trait]
    impl PolicyGate for AllowAllPolicy {
        async fn evaluate(
            &self,
            _request: RuntimePolicyRequest,
        ) -> Result<RuntimePolicyDecision, PortError> {
            Ok(RuntimePolicyDecision::Allow)
        }
    }

    struct RejectingCommandPort;

    #[async_trait]
    impl RuntimeCommandPort for RejectingCommandPort {
        async fn execute(
            &self,
            command: RuntimeCommand,
        ) -> Result<RuntimeCommandResult, PortError> {
            Ok(RuntimeCommandResult {
                command_id: command.command_id,
                status: RuntimeCommandStatus::Rejected,
                message: Some("not wired".to_string()),
            })
        }
    }

    struct AllowingCommandRouter;

    #[async_trait]
    impl RuntimeCommandRouter for AllowingCommandRouter {
        async fn route_command(
            &self,
            command: RuntimeCommand,
            _context: RuntimePolicyContext,
        ) -> Result<RuntimeCommandResult, PortError> {
            Ok(RuntimeCommandResult {
                command_id: command.command_id,
                status: RuntimeCommandStatus::Accepted,
                message: Some("routed".to_string()),
            })
        }
    }

    struct RecordingTerminalHost;

    #[async_trait]
    impl TerminalHostAdapter for RecordingTerminalHost {
        fn capabilities(&self) -> TerminalHostCapabilities {
            TerminalHostCapabilities {
                source: RuntimeSource::Headless,
                interactive_pty: false,
                scrollback: true,
                structured_activity: true,
                absolute_resize: true,
                out_of_process_lifecycle: false,
                replay: true,
            }
        }

        async fn spawn_pane(
            &self,
            spec: TerminalPaneSpawnSpec,
        ) -> Result<TerminalPaneHandle, PortError> {
            Ok(TerminalPaneHandle {
                session_id: spec.session_id,
                pane_id: spec.pane_id,
                generation: 1,
                source: RuntimeSource::Headless,
            })
        }

        async fn close_pane(&self, _handle: TerminalPaneHandle) -> Result<(), PortError> {
            Ok(())
        }

        async fn pane_handle(
            &self,
            session_id: &str,
            pane_id: &str,
        ) -> Result<TerminalPaneHandle, PortError> {
            Ok(TerminalPaneHandle {
                session_id: session_id.to_string(),
                pane_id: pane_id.to_string(),
                generation: 1,
                source: RuntimeSource::Headless,
            })
        }

        async fn pane_spawn_spec(
            &self,
            session_id: &str,
            pane_id: &str,
        ) -> Result<TerminalPaneSpawnSpec, PortError> {
            Ok(TerminalPaneSpawnSpec {
                session_id: session_id.to_string(),
                pane_id: pane_id.to_string(),
                kind: RuntimePaneKind::Shell,
                title: None,
                cwd: None,
                command: Vec::new(),
                env: BTreeMap::new(),
                rows: 24,
                cols: 80,
            })
        }

        async fn send_input(
            &self,
            _handle: TerminalPaneHandle,
            _bytes: Vec<u8>,
        ) -> Result<(), PortError> {
            Ok(())
        }

        async fn resize_pane(
            &self,
            _handle: TerminalPaneHandle,
            _size: TerminalResize,
        ) -> Result<(), PortError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn policy_and_command_ports_are_object_safe() {
        let policy: Box<dyn PolicyGate> = Box::new(AllowAllPolicy);
        let command_port: Box<dyn RuntimeCommandPort> = Box::new(RejectingCommandPort);
        let command_router: Box<dyn RuntimeCommandRouter> = Box::new(AllowingCommandRouter);
        let command = RuntimeCommand {
            command_id: "cmd".to_string(),
            target: RuntimeCommandTarget {
                session_id: "session".to_string(),
                pane_id: Some("pane".to_string()),
                generation: Some(1),
            },
            issued_at_ms: 1,
            kind: RuntimeCommandKind::Interrupt {
                reason: "test".to_string(),
            },
        };

        assert_eq!(
            policy
                .evaluate(RuntimePolicyRequest {
                    command: command.clone(),
                    context: RuntimePolicyContext::default(),
                })
                .await
                .expect("policy"),
            RuntimePolicyDecision::Allow
        );
        assert_eq!(
            command_port.execute(command).await.expect("command").status,
            RuntimeCommandStatus::Rejected
        );

        let routed = command_router
            .route_command(
                RuntimeCommand {
                    command_id: "cmd-route".to_string(),
                    target: RuntimeCommandTarget {
                        session_id: "session".to_string(),
                        pane_id: None,
                        generation: None,
                    },
                    issued_at_ms: 2,
                    kind: RuntimeCommandKind::ResolveApproval {
                        approval_id: "approval-1".to_string(),
                        approved: true,
                    },
                },
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route command");
        assert_eq!(routed.status, RuntimeCommandStatus::Accepted);
    }

    #[tokio::test]
    async fn terminal_host_adapter_is_object_safe() {
        let host: Box<dyn TerminalHostAdapter> = Box::new(RecordingTerminalHost);
        let handle = host
            .spawn_pane(TerminalPaneSpawnSpec {
                session_id: "session".to_string(),
                pane_id: "pane".to_string(),
                kind: RuntimePaneKind::Worker,
                title: Some("worker".to_string()),
                cwd: None,
                command: Vec::new(),
                env: BTreeMap::new(),
                rows: 40,
                cols: 120,
            })
            .await
            .expect("spawn");

        assert_eq!(host.capabilities().source, RuntimeSource::Headless);
        assert_eq!(handle.pane_id, "pane");
        host.resize_pane(
            handle.clone(),
            TerminalResize {
                rows: 50,
                cols: 160,
            },
        )
        .await
        .expect("resize");
        host.send_input(handle.clone(), b"hello".to_vec())
            .await
            .expect("input");
        host.close_pane(handle).await.expect("close");
    }
}
