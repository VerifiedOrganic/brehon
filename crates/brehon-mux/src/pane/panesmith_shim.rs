//! Brehon-owned adapter between pane metadata and Panesmith.
//!
//! Panesmith stays responsible for generic PTY, surface, input, transcript,
//! and event handling. Brehon keeps agent roles, task IDs, MCP state, prompt
//! delivery state, and supervisor/worker/reviewer workflow metadata outside
//! Panesmith and keys it by the existing string pane id.

use std::collections::{BTreeSet, HashMap};

use crate::error::{Error, Result};
use crate::pty::PtyConfig;

use panesmith::{
    AttachOptions, HostInput, OwnedPaneSnapshot, OwnedScrollbackSnapshot, PaneAttachOutcome,
    PaneAttachTerminal, PaneAttachTerminalControl, PaneConfig, PaneEventKind,
    PaneId as PanesmithPaneId, PaneManager, PaneManagerConfig, Size, TranscriptConfig,
    TranscriptMode,
};

#[cfg(test)]
pub(crate) const FORCE_PANESMITH_SPAWN_FAILURE_PANE_ID: &str = "__brehon_test_panesmith_spawn_fail";

/// Mirrored event data that can be applied to Brehon's pane/runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrehonPanesmithEvent {
    pub(crate) pane_id: String,
    pub(crate) panesmith_pane_id: PanesmithPaneId,
    pub(crate) seq: u64,
    pub(crate) kind: BrehonPanesmithEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BrehonPanesmithEventKind {
    Spawned,
    StateChanged,
    Output { bytes_len: usize },
    SurfaceChanged,
    InputSent { bytes_len: usize },
    Resized { rows: u16, cols: u16 },
    Exited { code: Option<i32> },
    Error { message: String },
    Other(&'static str),
}

/// Supervisor-only Panesmith owner for the first Brehon dogfood path.
pub(crate) struct BrehonPanesmithShim {
    manager: PaneManager,
    pane_ids: HashMap<String, PanesmithPaneId>,
    brehon_ids: HashMap<PanesmithPaneId, String>,
    snapshots: HashMap<String, OwnedPaneSnapshot>,
    scrollbacks: HashMap<String, OwnedScrollbackSnapshot>,
    last_seq: HashMap<PanesmithPaneId, u64>,
    next_pane_id: u64,
}

impl Default for BrehonPanesmithShim {
    fn default() -> Self {
        Self::new()
    }
}

impl BrehonPanesmithShim {
    pub(crate) fn new() -> Self {
        Self {
            manager: PaneManager::new(PaneManagerConfig::default()),
            pane_ids: HashMap::new(),
            brehon_ids: HashMap::new(),
            snapshots: HashMap::new(),
            scrollbacks: HashMap::new(),
            last_seq: HashMap::new(),
            next_pane_id: 0,
        }
    }

    pub(crate) fn spawn_supervisor(
        &mut self,
        pane_id: &str,
        config: &PtyConfig,
        title: &str,
    ) -> Result<PanesmithPaneId> {
        #[cfg(test)]
        if pane_id == FORCE_PANESMITH_SPAWN_FAILURE_PANE_ID {
            return Err(Error::pty("Panesmith: forced test spawn failure"));
        }

        if self.pane_ids.contains_key(pane_id) {
            return Err(Error::pty(format!(
                "Panesmith pane already registered for '{pane_id}'"
            )));
        }

        let panesmith_id = self.allocate_panesmith_id();
        let spawn_config = panesmith_spawn_config(config);
        let pane_config = to_panesmith_config(&spawn_config, panesmith_id, title);
        let spawned_id = self
            .manager
            .spawn(pane_config)
            .map_err(map_panesmith_error)?;

        self.pane_ids.insert(pane_id.to_string(), spawned_id);
        self.brehon_ids.insert(spawned_id, pane_id.to_string());
        self.refresh_cached_view_by_panesmith_id(spawned_id)?;
        Ok(spawned_id)
    }

    pub(crate) fn contains(&self, pane_id: &str) -> bool {
        self.pane_ids.contains_key(pane_id)
    }

    pub(crate) fn panesmith_id_for(&self, pane_id: &str) -> Option<PanesmithPaneId> {
        self.pane_ids.get(pane_id).copied()
    }

    #[cfg(test)]
    pub(crate) fn brehon_id_for(&self, pane_id: PanesmithPaneId) -> Option<&str> {
        self.brehon_ids.get(&pane_id).map(String::as_str)
    }

    pub(crate) fn snapshot(&self, pane_id: &str) -> Option<&OwnedPaneSnapshot> {
        self.snapshots.get(pane_id)
    }

    pub(crate) fn scrollback(&self, pane_id: &str) -> Option<&OwnedScrollbackSnapshot> {
        self.scrollbacks.get(pane_id)
    }

    pub(crate) fn send_input_bytes(&mut self, pane_id: &str, bytes: &[u8]) -> Result<()> {
        let panesmith_id = self
            .panesmith_id_for(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        self.manager
            .send_input(panesmith_id, HostInput::Raw(bytes.to_vec()))
            .map_err(map_panesmith_error)
    }

    pub(crate) fn resize(&mut self, pane_id: &str, rows: u16, cols: u16) -> Result<bool> {
        let Some(panesmith_id) = self.panesmith_id_for(pane_id) else {
            return Ok(false);
        };
        self.manager
            .resize(panesmith_id, Size::new(rows, cols))
            .map_err(map_panesmith_error)?;
        self.refresh_cached_view_by_panesmith_id(panesmith_id)?;
        Ok(true)
    }

    pub(crate) fn kill_and_forget(&mut self, pane_id: &str) -> Result<bool> {
        let Some(panesmith_id) = self.pane_ids.remove(pane_id) else {
            return Ok(false);
        };
        self.brehon_ids.remove(&panesmith_id);
        self.snapshots.remove(pane_id);
        self.scrollbacks.remove(pane_id);
        self.last_seq.remove(&panesmith_id);
        let _ = self
            .manager
            .kill(panesmith_id, panesmith::KillReason::HostRequested);
        Ok(true)
    }

    pub(crate) fn attach_blocking<Terminal, Control>(
        &mut self,
        pane_id: &str,
        options: AttachOptions,
        terminal: &mut Terminal,
        control: &mut Control,
    ) -> Result<PaneAttachOutcome>
    where
        Terminal: PaneAttachTerminal,
        Control: PaneAttachTerminalControl,
    {
        let panesmith_id = self
            .panesmith_id_for(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        let outcome = self
            .manager
            .attach_blocking(panesmith_id, options, terminal, control)
            .map_err(map_panesmith_attach_error)?;
        self.refresh_cached_view_by_panesmith_id(panesmith_id)?;
        Ok(outcome)
    }

    #[cfg(test)]
    pub(crate) fn last_seq_for_brehon(&mut self, pane_id: &str) -> Result<u64> {
        let panesmith_id = self
            .panesmith_id_for(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        let seq = self
            .manager
            .last_seq(panesmith_id)
            .map_err(map_panesmith_error)?;
        self.last_seq.insert(panesmith_id, seq);
        Ok(seq)
    }

    pub(crate) fn drain_events(&mut self) -> Vec<BrehonPanesmithEvent> {
        let mut events = Vec::new();
        self.manager.drain_events(&mut events);

        let mut affected = BTreeSet::new();
        let mut mirrored = Vec::with_capacity(events.len());
        for event in events {
            self.last_seq.insert(event.pane_id, event.seq);
            let Some(brehon_id) = self.brehon_ids.get(&event.pane_id).cloned() else {
                continue;
            };
            affected.insert(event.pane_id);
            mirrored.push(BrehonPanesmithEvent {
                pane_id: brehon_id,
                panesmith_pane_id: event.pane_id,
                seq: event.seq,
                kind: mirror_event_kind(&event.kind),
            });
        }

        for panesmith_id in affected {
            if let Err(err) = self.refresh_cached_view_by_panesmith_id(panesmith_id) {
                tracing::warn!(
                    pane_id = panesmith_id.get(),
                    error = %err,
                    "Failed to refresh Panesmith cached view after event drain"
                );
            }
            match self.manager.last_seq(panesmith_id) {
                Ok(seq) => {
                    self.last_seq.insert(panesmith_id, seq);
                }
                Err(err) => {
                    tracing::warn!(
                        pane_id = panesmith_id.get(),
                        error = %err,
                        "Failed to read Panesmith pane sequence after event drain"
                    );
                }
            }
        }

        mirrored
    }

    fn allocate_panesmith_id(&mut self) -> PanesmithPaneId {
        self.next_pane_id = self.next_pane_id.saturating_add(1);
        PanesmithPaneId::new(self.next_pane_id)
    }

    fn refresh_cached_view_by_panesmith_id(&mut self, panesmith_id: PanesmithPaneId) -> Result<()> {
        let Some(brehon_id) = self.brehon_ids.get(&panesmith_id).cloned() else {
            return Ok(());
        };
        let snapshot = self
            .manager
            .snapshot(panesmith_id)
            .map_err(map_panesmith_error)?
            .to_owned_snapshot();
        let scrollback = self
            .manager
            .scrollback(panesmith_id)
            .map_err(map_panesmith_error)?
            .to_owned_snapshot();
        self.snapshots.insert(brehon_id.clone(), snapshot);
        self.scrollbacks.insert(brehon_id, scrollback);
        Ok(())
    }
}

pub(crate) fn to_panesmith_config(
    config: &PtyConfig,
    pane_id: PanesmithPaneId,
    title: &str,
) -> PaneConfig {
    let mut pane = PaneConfig::command_with_args(config.command.clone(), config.args.clone())
        .with_id(pane_id)
        .with_size(Size::new(config.rows, config.cols))
        .with_title(title.to_string())
        .with_transcript(TranscriptConfig::new(TranscriptMode::Both));

    if let Some(cwd) = &config.cwd {
        pane = pane.with_cwd(cwd.clone());
    }

    for (key, value) in &config.env {
        pane = pane.with_env(key.clone(), value.clone());
    }

    pane
}

fn mirror_event_kind(kind: &PaneEventKind) -> BrehonPanesmithEventKind {
    match kind {
        PaneEventKind::Spawned(_) => BrehonPanesmithEventKind::Spawned,
        PaneEventKind::StateChanged(_) => BrehonPanesmithEventKind::StateChanged,
        PaneEventKind::Output(output) => BrehonPanesmithEventKind::Output {
            bytes_len: output.bytes_len,
        },
        PaneEventKind::SurfaceChanged(_) => BrehonPanesmithEventKind::SurfaceChanged,
        PaneEventKind::InputSent(input) => BrehonPanesmithEventKind::InputSent {
            bytes_len: input.bytes_len,
        },
        PaneEventKind::Resized(resized) => BrehonPanesmithEventKind::Resized {
            rows: resized.size.rows,
            cols: resized.size.cols,
        },
        PaneEventKind::Exited(exited) => BrehonPanesmithEventKind::Exited { code: exited.code },
        PaneEventKind::Error(error) => BrehonPanesmithEventKind::Error {
            message: error.error.to_string(),
        },
        PaneEventKind::AttachStarted(_) => BrehonPanesmithEventKind::Other("attach_started"),
        PaneEventKind::AttachEnded(_) => BrehonPanesmithEventKind::Other("attach_ended"),
        PaneEventKind::TranscriptRotated(_) => {
            BrehonPanesmithEventKind::Other("transcript_rotated")
        }
        PaneEventKind::Overflow(_) => BrehonPanesmithEventKind::Other("overflow"),
    }
}

fn map_panesmith_error(err: panesmith::PaneError) -> Error {
    Error::pty(format!("Panesmith: {err}"))
}

fn map_panesmith_attach_error(err: panesmith::PaneAttachError) -> Error {
    Error::pty(format!("Panesmith attach: {err}"))
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn panesmith_spawn_config(config: &PtyConfig) -> PtyConfig {
    let mut config = config.clone();
    if is_test_agent_command(&config.command) {
        config.command = "sh".to_string();
        config.args = vec!["-c".to_string(), "cat".to_string()];
    }
    config
}

#[cfg(not(any(test, feature = "test-pty-fallback")))]
fn panesmith_spawn_config(config: &PtyConfig) -> PtyConfig {
    config.clone()
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn is_test_agent_command(command: &str) -> bool {
    matches!(
        command,
        "claude" | "codex" | "copilot" | "gemini" | "gh" | "junie" | "kimi" | "opencode" | "agy"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    fn test_config(command: &str, args: &[&str]) -> PtyConfig {
        PtyConfig {
            command: command.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            cwd: Some(PathBuf::from("/tmp")),
            env: vec![("TEST_KEY".to_string(), "TEST_VALUE".to_string())],
            rows: 9,
            cols: 31,
        }
    }

    #[test]
    fn maps_pty_config_to_panesmith_config_without_brehon_metadata() {
        let config = test_config("sh", &["-c", "true"]);
        let pane_config = to_panesmith_config(&config, PanesmithPaneId::new(17), "supervisor");

        assert_eq!(pane_config.id, Some(PanesmithPaneId::new(17)));
        assert_eq!(pane_config.program(), "sh");
        assert_eq!(pane_config.args(), ["-c", "true"]);
        assert_eq!(pane_config.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(
            pane_config.env.get("TEST_KEY").map(String::as_str),
            Some("TEST_VALUE")
        );
        assert_eq!(pane_config.size, Size::new(9, 31));
        assert_eq!(pane_config.title.as_deref(), Some("supervisor"));
        assert_eq!(pane_config.transcript.mode, TranscriptMode::Both);
    }

    #[test]
    fn maintains_string_to_numeric_pane_id_mapping() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("sh", &["-c", "true"]);

        let panesmith_id = shim
            .spawn_supervisor("supervisor", &config, "Supervisor")
            .expect("spawn Panesmith supervisor");

        assert_eq!(shim.panesmith_id_for("supervisor"), Some(panesmith_id));
        assert_eq!(shim.brehon_id_for(panesmith_id), Some("supervisor"));
        assert!(shim.snapshot("supervisor").is_some());
    }

    #[test]
    fn drains_events_and_preserves_pane_sequence() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("sh", &["-c", "printf shim-output"]);
        shim.spawn_supervisor("supervisor", &config, "Supervisor")
            .expect("spawn Panesmith supervisor");

        let mut mirrored = Vec::new();
        for _ in 0..50 {
            mirrored.extend(shim.drain_events());
            if mirrored.iter().any(|event| {
                matches!(event.kind, BrehonPanesmithEventKind::Output { bytes_len } if bytes_len > 0)
            }) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            mirrored.iter().any(|event| matches!(
                event.kind,
                BrehonPanesmithEventKind::Spawned | BrehonPanesmithEventKind::StateChanged
            )),
            "expected spawn/state events, got {mirrored:?}"
        );
        assert!(
            mirrored.iter().any(|event| {
                matches!(event.kind, BrehonPanesmithEventKind::Output { bytes_len } if bytes_len > 0)
            }),
            "expected output event, got {mirrored:?}"
        );

        let last_seq = shim
            .last_seq_for_brehon("supervisor")
            .expect("last seq should be readable");
        assert!(last_seq >= mirrored.iter().map(|event| event.seq).max().unwrap_or(0));
    }

    #[test]
    fn failed_spawn_leaves_mapping_empty_for_fallback() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("/definitely/missing/panesmith-command", &[]);

        let err = shim
            .spawn_supervisor("supervisor", &config, "Supervisor")
            .expect_err("missing command should fail");

        assert!(err.to_string().contains("Panesmith"));
        assert!(shim.panesmith_id_for("supervisor").is_none());
        assert!(shim.snapshot("supervisor").is_none());
    }
}
