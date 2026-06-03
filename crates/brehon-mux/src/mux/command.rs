//! Runtime command queue and mux-owned command execution.

use std::path::PathBuf;

use async_trait::async_trait;
use brehon_ports::{PortError, RuntimeCommandPort};
use brehon_types::{
    PromptDeliveryMode, RuntimeCommand, RuntimeCommandKind, RuntimeCommandResult,
    RuntimeCommandStatus, RuntimePaneKind, RuntimePolicyDecision,
};
use tokio::sync::{mpsc, oneshot};

use super::{Mux, PromptDeliveryAttempt};
use crate::pane::DeathReason;

const DEFAULT_RUNTIME_COMMAND_CHANNEL_CAPACITY: usize = 128;

/// Runtime command port that submits approved commands to the mux owner.
///
/// `Mux` is intentionally owned by the TUI/event-loop thread because several
/// pane backends expose non-Send PTY futures. This port gives the daemon a
/// Send + Sync command boundary without moving mux state behind a cross-thread
/// lock or weakening the core runtime port trait.
#[derive(Clone)]
pub struct MuxRuntimeCommandPort {
    tx: mpsc::Sender<MuxRuntimeCommandRequest>,
}

/// Receives runtime command requests on the mux owner thread.
pub struct MuxRuntimeCommandReceiver {
    rx: mpsc::Receiver<MuxRuntimeCommandRequest>,
}

/// One command request waiting for mux-local execution.
pub struct MuxRuntimeCommandRequest {
    command: RuntimeCommand,
    response_tx: oneshot::Sender<RuntimeCommandResult>,
}

impl MuxRuntimeCommandPort {
    pub fn channel(capacity: usize) -> (Self, MuxRuntimeCommandReceiver) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, MuxRuntimeCommandReceiver { rx })
    }

    pub fn channel_default() -> (Self, MuxRuntimeCommandReceiver) {
        Self::channel(DEFAULT_RUNTIME_COMMAND_CHANNEL_CAPACITY)
    }
}

#[async_trait]
impl RuntimeCommandPort for MuxRuntimeCommandPort {
    async fn execute(&self, command: RuntimeCommand) -> Result<RuntimeCommandResult, PortError> {
        let command_id = command.command_id.clone();
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(MuxRuntimeCommandRequest {
                command,
                response_tx,
            })
            .await
            .map_err(|_| PortError::Runtime("mux command receiver is closed".to_string()))?;
        response_rx.await.map_err(|_| {
            PortError::Runtime(format!(
                "mux command '{command_id}' was dropped before completion"
            ))
        })
    }
}

impl MuxRuntimeCommandReceiver {
    pub fn try_recv(&mut self) -> Result<MuxRuntimeCommandRequest, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }

    pub async fn recv(&mut self) -> Option<MuxRuntimeCommandRequest> {
        self.rx.recv().await
    }
}

impl MuxRuntimeCommandRequest {
    pub fn command(&self) -> &RuntimeCommand {
        &self.command
    }

    pub fn complete(self, result: RuntimeCommandResult) {
        let _ = self.response_tx.send(result);
    }
}

impl Mux {
    /// Execute one daemon-approved command on the mux owner thread.
    ///
    /// The daemon is responsible for asynchronous policy routing. This method
    /// keeps a synchronous final guard around mux invariants, pane generation
    /// checks, and local policy hooks before mutating pane state.
    pub fn execute_runtime_command(
        &mut self,
        rt: &tokio::runtime::Handle,
        command: RuntimeCommand,
    ) -> RuntimeCommandResult {
        let command_id = command.command_id.clone();
        if let Err(reason) = self.validate_command_session(&command) {
            return rejected(command_id, reason);
        }

        match command.kind.clone() {
            RuntimeCommandKind::SendPrompt {
                text,
                from,
                delivery,
                ..
            } => {
                let pane_id = match self.validated_target_pane(&command, "send prompt") {
                    Ok(pane_id) => pane_id,
                    Err(reason) => return rejected(command_id, reason),
                };
                match delivery {
                    PromptDeliveryMode::Attempt => match rt.block_on(self.attempt_prompt_delivery(
                        &pane_id,
                        &text,
                        from.as_deref(),
                    )) {
                        Ok(PromptDeliveryAttempt::Delivered { .. }) => {
                            applied(command_id, "prompt delivered")
                        }
                        Ok(PromptDeliveryAttempt::Queued {
                            prompt_id,
                            ahead_of,
                        }) => RuntimeCommandResult {
                            command_id,
                            status: RuntimeCommandStatus::Deferred,
                            message: Some(format!(
                                "prompt {prompt_id} deferred with {ahead_of} prompt(s) ahead"
                            )),
                        },
                        Ok(PromptDeliveryAttempt::AlreadyPresent {
                            prompt_id,
                            position,
                        }) => RuntimeCommandResult {
                            command_id,
                            status: RuntimeCommandStatus::Deferred,
                            message: Some(format!(
                                "prompt {prompt_id} already queued at {position}"
                            )),
                        },
                        Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                            rejected(command_id, format!("{reason:?}"))
                        }
                        Err(err) => rejected(command_id, err.to_string()),
                    },
                    PromptDeliveryMode::Enqueue => {
                        match rt.block_on(self.deliver_prompt(&pane_id, &text, from.as_deref())) {
                            Ok(()) => applied(command_id, "prompt accepted for delivery"),
                            Err(err) => rejected(command_id, err.to_string()),
                        }
                    }
                    PromptDeliveryMode::Direct => match rt.block_on(self.inject(&pane_id, &text)) {
                        Ok(()) => applied(command_id, "prompt injected directly"),
                        Err(err) => rejected(command_id, err.to_string()),
                    },
                }
            }
            RuntimeCommandKind::BroadcastPrompt { text, pane_ids, .. } => {
                if pane_ids.is_empty() {
                    return rejected(command_id, "broadcast requires at least one pane");
                }
                for pane_id in &pane_ids {
                    if self.get(pane_id).is_none() {
                        return rejected(
                            command_id,
                            format!("broadcast target pane '{pane_id}' was not found"),
                        );
                    }
                }
                for pane_id in &pane_ids {
                    if let Err(err) = rt.block_on(self.deliver_prompt(pane_id, &text, None)) {
                        return rejected(
                            command_id,
                            format!("broadcast delivery to '{pane_id}' failed: {err}"),
                        );
                    }
                }
                applied(
                    command_id,
                    format!("broadcast accepted for {} pane(s)", pane_ids.len()),
                )
            }
            RuntimeCommandKind::SendTerminalInput { bytes } => {
                let pane_id = match self.validated_target_pane(&command, "terminal input") {
                    Ok(pane_id) => pane_id,
                    Err(reason) => return rejected(command_id, reason),
                };
                match rt.block_on(self.send_input_to(&pane_id, &bytes)) {
                    Ok(()) => applied(command_id, "terminal input sent"),
                    Err(err) => rejected(command_id, err.to_string()),
                }
            }
            RuntimeCommandKind::Interrupt { .. } => {
                let pane_id = match self.validated_target_pane(&command, "interrupt") {
                    Ok(pane_id) => pane_id,
                    Err(reason) => return rejected(command_id, reason),
                };
                match rt.block_on(self.interrupt(&pane_id)) {
                    Ok(()) => applied(command_id, "pane interrupted"),
                    Err(err) => rejected(command_id, err.to_string()),
                }
            }
            RuntimeCommandKind::ResetPane { .. } => {
                let pane_id = match self.validated_target_pane(&command, "reset") {
                    Ok(pane_id) => pane_id,
                    Err(reason) => return rejected(command_id, reason),
                };
                match rt.block_on(self.recycle_pane(&pane_id)) {
                    Ok(()) => applied(command_id, "pane reset requested"),
                    Err(err) => rejected(command_id, err.to_string()),
                }
            }
            RuntimeCommandKind::RecyclePane { reason } => {
                let pane_id = match self.validated_target_pane(&command, "recycle") {
                    Ok(pane_id) => pane_id,
                    Err(reason) => return rejected(command_id, reason),
                };
                let generation = rt.block_on(self.recycle(&pane_id, &reason));
                applied(
                    command_id,
                    format!("pane recycled to generation {}", generation.0),
                )
            }
            RuntimeCommandKind::QuarantinePane { reason } => {
                let pane_id = match self.validated_target_pane(&command, "quarantine") {
                    Ok(pane_id) => pane_id,
                    Err(reason) => return rejected(command_id, reason),
                };
                let outcome = self.quarantine(&pane_id, DeathReason::Quarantined(reason));
                applied(
                    command_id,
                    format!(
                        "pane quarantined: already_dead={}, reason={:?}",
                        outcome.was_already_dead, outcome.new_reason
                    ),
                )
            }
            RuntimeCommandKind::SpawnPane {
                kind,
                pane_id,
                title,
                cwd,
                ..
            } => match self.execute_runtime_spawn(kind, pane_id, title, cwd) {
                Ok(message) => applied(command_id, message),
                Err(err) => rejected(command_id, err.to_string()),
            },
            RuntimeCommandKind::ResizePane { .. } => rejected(
                command_id,
                "pane resize is only implemented by terminal host adapters",
            ),
            RuntimeCommandKind::ClosePane { .. } => {
                let pane_id = match command.target.pane_id.as_deref() {
                    Some(pane_id) => pane_id.to_string(),
                    None => return rejected(command_id, "close requires a pane target"),
                };
                if let Err(reason) = self.validate_target_generation(&command, &pane_id) {
                    return rejected(command_id, reason);
                }
                match self.evaluate_close_policy(&command, &pane_id) {
                    RuntimePolicyDecision::Allow => {}
                    decision => {
                        return rejected(
                            command_id,
                            format!(
                                "policy rejected close: {:?}",
                                Self::policy_rejection_reason(&decision)
                            ),
                        );
                    }
                }
                if self.remove_pane(&pane_id).is_some() {
                    applied(command_id, format!("pane '{pane_id}' closed"))
                } else {
                    applied(command_id, format!("pane '{pane_id}' was already absent"))
                }
            }
            RuntimeCommandKind::ResolveApproval { .. } => rejected(
                command_id,
                "approval resolution is not backed by mux state yet",
            ),
        }
    }

    fn validate_command_session(&self, command: &RuntimeCommand) -> Result<(), String> {
        let Some(session_name) = self.session_name.as_deref() else {
            return Ok(());
        };
        if command.target.session_id == session_name {
            Ok(())
        } else {
            Err(format!(
                "command targets session '{}' but mux owns session '{session_name}'",
                command.target.session_id
            ))
        }
    }

    fn validated_target_pane(
        &self,
        command: &RuntimeCommand,
        operation: &str,
    ) -> Result<String, String> {
        let pane_id = command
            .target
            .pane_id
            .as_deref()
            .ok_or_else(|| format!("{operation} requires a pane target"))?;
        let pane = self
            .get(pane_id)
            .ok_or_else(|| format!("{operation} target pane '{pane_id}' was not found"))?;
        if let Some(expected_generation) = command.target.generation
            && pane.current_generation().0 != expected_generation
        {
            return Err(format!(
                "{operation} target pane '{pane_id}' is at generation {}, command expected {expected_generation}",
                pane.current_generation().0
            ));
        }
        Ok(pane_id.to_string())
    }

    fn validate_target_generation(
        &self,
        command: &RuntimeCommand,
        pane_id: &str,
    ) -> Result<(), String> {
        let Some(expected_generation) = command.target.generation else {
            return Ok(());
        };
        let Some(pane) = self.get(pane_id) else {
            return Ok(());
        };
        if pane.current_generation().0 == expected_generation {
            Ok(())
        } else {
            Err(format!(
                "target pane '{pane_id}' is at generation {}, command expected {expected_generation}",
                pane.current_generation().0
            ))
        }
    }

    fn evaluate_close_policy(
        &self,
        command: &RuntimeCommand,
        pane_id: &str,
    ) -> RuntimePolicyDecision {
        if self.get(pane_id).is_none() {
            return RuntimePolicyDecision::Allow;
        }
        self.evaluate_runtime_policy_immediate(
            command.clone(),
            self.runtime_policy_context_for_pane(pane_id),
        )
    }

    fn execute_runtime_spawn(
        &mut self,
        kind: RuntimePaneKind,
        pane_id: Option<String>,
        title: Option<String>,
        cwd: Option<String>,
    ) -> crate::Result<String> {
        let pane_id = pane_id
            .or(title)
            .ok_or_else(|| crate::Error::pty("spawn requires pane_id or title"))?;
        match kind {
            RuntimePaneKind::Shell => {
                let cwd = cwd
                    .map(PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let id = self.add_shell(&pane_id, cwd, None)?;
                Ok(format!("shell pane '{id}' spawned"))
            }
            other => Err(crate::Error::pty(format!(
                "runtime spawn for pane kind {other:?} is not implemented"
            ))),
        }
    }
}

fn applied(command_id: String, message: impl Into<String>) -> RuntimeCommandResult {
    RuntimeCommandResult {
        command_id,
        status: RuntimeCommandStatus::Applied,
        message: Some(message.into()),
    }
}

fn rejected(command_id: String, message: impl Into<String>) -> RuntimeCommandResult {
    RuntimeCommandResult {
        command_id,
        status: RuntimeCommandStatus::Rejected,
        message: Some(message.into()),
    }
}
