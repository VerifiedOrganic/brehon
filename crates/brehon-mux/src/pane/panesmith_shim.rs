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
    AttachOptions, InputKind, InputOutcome, InputTransaction, OwnedPaneSnapshot,
    OwnedScrollbackSnapshot, PaneAttachOutcome, PaneAttachTerminal, PaneAttachTerminalControl,
    PaneConfig, PaneEventKind, PaneId as PanesmithPaneId, PaneManager, PaneManagerConfig,
    ScrollbackConfig, Size, TranscriptConfig, TranscriptMode,
};

#[cfg(test)]
pub(crate) const FORCE_PANESMITH_SPAWN_FAILURE_PANE_ID: &str = "__brehon_test_panesmith_spawn_fail";

const BREHON_PANESMITH_SCROLLBACK_LINES: usize = 500;
const BREHON_PANESMITH_SCROLLBACK_BYTES: usize = 2 * 1024 * 1024;
const BREHON_PANESMITH_EVENT_LOG_EVENTS: usize = 2_000;
const BREHON_PANESMITH_MAX_PTY_FRAMES_PER_DRAIN: usize = 4;

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
    Output {
        bytes_len: usize,
    },
    SurfaceChanged,
    InputSent {
        input_kind: InputKind,
        bytes_len: usize,
        recorded: bool,
    },
    Resized {
        rows: u16,
        cols: u16,
    },
    Exited {
        code: Option<i32>,
    },
    Error {
        message: String,
    },
    Other(&'static str),
}

/// Panesmith owner for Brehon-managed interactive PTY panes.
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
        Self::new_with_manager_config(
            PaneManagerConfig::default()
                .with_default_scrollback(brehon_panesmith_scrollback_config())
                .with_max_event_log_entries(brehon_panesmith_event_log_entries())
                .with_max_pty_frames_per_drain(brehon_panesmith_max_pty_frames_per_drain()),
        )
    }

    fn new_with_manager_config(manager_config: PaneManagerConfig) -> Self {
        Self {
            manager: PaneManager::new(manager_config),
            pane_ids: HashMap::new(),
            brehon_ids: HashMap::new(),
            snapshots: HashMap::new(),
            scrollbacks: HashMap::new(),
            last_seq: HashMap::new(),
            next_pane_id: 0,
        }
    }

    #[cfg(test)]
    fn new_with_event_log_entries_for_test(max_events: usize) -> Self {
        Self::new_with_manager_config(
            PaneManagerConfig::default()
                .with_default_scrollback(brehon_panesmith_scrollback_config())
                .with_max_event_log_entries(max_events),
        )
    }

    pub(crate) fn spawn_pane(
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

    pub(crate) fn refresh_scrollback(&mut self, pane_id: &str) -> Result<bool> {
        let Some(panesmith_id) = self.panesmith_id_for(pane_id) else {
            return Ok(false);
        };
        let scrollback = self
            .manager
            .scrollback(panesmith_id)
            .map_err(map_panesmith_error)?
            .to_owned_snapshot();
        self.scrollbacks.insert(pane_id.to_string(), scrollback);
        Ok(true)
    }

    pub(crate) fn clear_scrollback(&mut self, pane_id: &str) {
        self.scrollbacks.remove(pane_id);
    }

    pub(crate) fn refresh_snapshot(&mut self, pane_id: &str) -> Result<bool> {
        let Some(panesmith_id) = self.panesmith_id_for(pane_id) else {
            return Ok(false);
        };
        self.refresh_cached_view_by_panesmith_id(panesmith_id)?;
        Ok(true)
    }

    pub(crate) fn send_input_transaction(
        &mut self,
        pane_id: &str,
        transaction: InputTransaction,
    ) -> Result<InputOutcome> {
        let panesmith_id = self
            .panesmith_id_for(pane_id)
            .ok_or_else(|| Error::pane_not_found(pane_id))?;
        let outcome = self
            .manager
            .send_input_transaction(panesmith_id, transaction)
            .map_err(map_panesmith_error)?;
        self.refresh_cached_view_by_panesmith_id(panesmith_id)?;
        Ok(outcome)
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
        let Some(panesmith_id) = self.pane_ids.get(pane_id).copied() else {
            return Ok(false);
        };

        self.manager
            .kill_and_remove(panesmith_id, panesmith::KillReason::HostRequested)
            .map_err(map_panesmith_error)?;

        self.pane_ids.remove(pane_id);
        self.brehon_ids.remove(&panesmith_id);
        self.snapshots.remove(pane_id);
        self.scrollbacks.remove(pane_id);
        self.last_seq.remove(&panesmith_id);
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

    pub(crate) fn drain_events(
        &mut self,
        snapshot_panes: Option<&BTreeSet<String>>,
    ) -> Vec<BrehonPanesmithEvent> {
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
            let should_refresh_snapshot =
                self.brehon_ids.get(&panesmith_id).is_some_and(|brehon_id| {
                    snapshot_panes.is_none_or(|panes| panes.contains(brehon_id))
                });
            if should_refresh_snapshot {
                if let Err(err) = self.refresh_cached_view_by_panesmith_id(panesmith_id) {
                    tracing::warn!(
                        pane_id = panesmith_id.get(),
                        error = %err,
                        "Failed to refresh Panesmith cached view after event drain"
                    );
                }
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
        self.snapshots.insert(brehon_id.clone(), snapshot);
        if self.scrollbacks.contains_key(&brehon_id) {
            let scrollback = self
                .manager
                .scrollback(panesmith_id)
                .map_err(map_panesmith_error)?
                .to_owned_snapshot();
            self.scrollbacks.insert(brehon_id, scrollback);
        }
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
        .with_scrollback(brehon_panesmith_scrollback_config())
        .with_transcript(TranscriptConfig::new(TranscriptMode::Both));

    if let Some(cwd) = &config.cwd {
        pane = pane.with_cwd(cwd.clone());
    }

    for (key, value) in &config.env {
        pane = pane.with_env(key.clone(), value.clone());
    }

    pane
}

fn brehon_panesmith_scrollback_config() -> ScrollbackConfig {
    let lines = std::env::var("BREHON_PANESMITH_SCROLLBACK_LINES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|lines| *lines > 0)
        .unwrap_or(BREHON_PANESMITH_SCROLLBACK_LINES);
    ScrollbackConfig::new(lines, BREHON_PANESMITH_SCROLLBACK_BYTES)
        .expect("Brehon Panesmith scrollback limits are non-zero")
}

fn brehon_panesmith_event_log_entries() -> usize {
    std::env::var("BREHON_PANESMITH_EVENT_LOG_EVENTS")
        .ok()
        .as_deref()
        .and_then(parse_panesmith_event_log_entries)
        .unwrap_or(BREHON_PANESMITH_EVENT_LOG_EVENTS)
}

fn parse_panesmith_event_log_entries(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()
}

fn brehon_panesmith_max_pty_frames_per_drain() -> usize {
    std::env::var("BREHON_PANESMITH_MAX_PTY_FRAMES_PER_DRAIN")
        .ok()
        .as_deref()
        .and_then(parse_positive_usize)
        .unwrap_or(BREHON_PANESMITH_MAX_PTY_FRAMES_PER_DRAIN)
}

fn parse_positive_usize(value: &str) -> Option<usize> {
    value
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
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
            input_kind: input.input_kind,
            bytes_len: input.bytes_len,
            recorded: input.recorded,
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
        assert_eq!(
            pane_config.scrollback,
            Some(brehon_panesmith_scrollback_config())
        );
        assert_eq!(pane_config.transcript.mode, TranscriptMode::Both);
    }

    #[test]
    fn maintains_string_to_numeric_pane_id_mapping() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("sh", &["-c", "true"]);

        let panesmith_id = shim
            .spawn_pane("supervisor", &config, "Supervisor")
            .expect("spawn Panesmith supervisor");

        assert_eq!(shim.panesmith_id_for("supervisor"), Some(panesmith_id));
        assert_eq!(shim.brehon_id_for(panesmith_id), Some("supervisor"));
        assert!(shim.snapshot("supervisor").is_some());
    }

    #[test]
    fn kill_and_forget_removes_pane_from_panesmith_manager() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("sh", &["-c", "sleep 30"]);

        let panesmith_id = shim
            .spawn_pane("supervisor", &config, "Supervisor")
            .expect("spawn Panesmith supervisor");

        assert!(shim.manager.snapshot(panesmith_id).is_ok());
        assert!(
            shim.kill_and_forget("supervisor")
                .expect("kill and remove Panesmith supervisor")
        );

        assert!(shim.panesmith_id_for("supervisor").is_none());
        assert!(shim.brehon_id_for(panesmith_id).is_none());
        assert!(shim.snapshot("supervisor").is_none());
        assert!(matches!(
            shim.manager.snapshot(panesmith_id),
            Err(panesmith::PaneError::NotFound { pane_id }) if pane_id == panesmith_id
        ));
    }

    #[test]
    fn repeated_kill_and_forget_cycles_do_not_retain_old_panes() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("sh", &["-c", "sleep 30"]);
        let mut removed_ids = Vec::new();

        for _ in 0..3 {
            let panesmith_id = shim
                .spawn_pane("supervisor", &config, "Supervisor")
                .expect("spawn Panesmith supervisor");
            assert!(
                shim.kill_and_forget("supervisor")
                    .expect("kill and remove Panesmith supervisor")
            );
            removed_ids.push(panesmith_id);
        }

        for panesmith_id in removed_ids {
            assert!(matches!(
                shim.manager.snapshot(panesmith_id),
                Err(panesmith::PaneError::NotFound { pane_id }) if pane_id == panesmith_id
            ));
        }
    }

    #[test]
    fn drains_events_and_preserves_pane_sequence() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("sh", &["-c", "printf shim-output"]);
        shim.spawn_pane("supervisor", &config, "Supervisor")
            .expect("spawn Panesmith supervisor");

        let mut mirrored = Vec::new();
        for _ in 0..50 {
            mirrored.extend(shim.drain_events(None));
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
    fn panesmith_event_log_retention_is_bounded() {
        let mut shim = BrehonPanesmithShim::new_with_event_log_entries_for_test(3);
        let config = test_config("sh", &["-c", "sleep 30"]);
        let panesmith_id = shim
            .spawn_pane("supervisor", &config, "Supervisor")
            .expect("spawn Panesmith supervisor");

        for cols in 31..36 {
            shim.manager
                .resize(panesmith_id, Size::new(9, cols))
                .expect("resize should emit events");
        }

        let mut live_events = Vec::new();
        shim.manager.drain_events(&mut live_events);
        assert!(
            live_events.len() > 3,
            "live event queue must still receive the full stream"
        );

        let dump = shim
            .manager
            .dump_repro(panesmith_id, panesmith::ReproDumpOptions::default())
            .expect("bounded event history should still dump repros");
        assert!(
            dump.events.len() <= 3,
            "retained event log exceeded configured cap: {:?}",
            dump.events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>()
        );
        assert!(
            dump.event_log_events_dropped > 0,
            "bounded event history should record dropped retained events"
        );

        shim.kill_and_forget("supervisor")
            .expect("cleanup Panesmith supervisor");
    }

    #[test]
    fn parses_panesmith_event_log_limit() {
        assert_eq!(parse_panesmith_event_log_entries("0"), Some(0));
        assert_eq!(parse_panesmith_event_log_entries(" 42 "), Some(42));
        assert_eq!(parse_panesmith_event_log_entries("nope"), None);
        assert_eq!(parse_panesmith_event_log_entries(""), None);
    }

    #[test]
    fn parses_panesmith_drain_limit() {
        assert_eq!(parse_positive_usize("1"), Some(1));
        assert_eq!(parse_positive_usize(" 4 "), Some(4));
        assert_eq!(parse_positive_usize("0"), None);
        assert_eq!(parse_positive_usize("nope"), None);
        assert_eq!(parse_positive_usize(""), None);
    }

    #[test]
    fn hidden_pane_drain_defers_owned_snapshot_refresh() {
        let mut shim = BrehonPanesmithShim::new();
        let marker = "HIDDEN_REFRESH_MARKER";
        let command = format!("sleep 0.2; printf {marker}; sleep 1");
        let config = test_config("sh", &["-c", &command]);
        shim.spawn_pane("hidden-worker", &config, "Hidden")
            .expect("spawn Panesmith pane");

        let visible_panes = BTreeSet::new();
        let mut saw_output = false;
        for _ in 0..50 {
            let events = shim.drain_events(Some(&visible_panes));
            saw_output |= events.iter().any(|event| {
                matches!(event.kind, BrehonPanesmithEventKind::Output { bytes_len } if bytes_len > 0)
            });
            if saw_output {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_output, "expected hidden pane output");
        assert!(
            !snapshot_text(shim.snapshot("hidden-worker").expect("initial snapshot"))
                .contains(marker),
            "hidden pane snapshot should not refresh during background drain"
        );

        assert!(
            shim.refresh_snapshot("hidden-worker")
                .expect("refresh visible snapshot")
        );
        assert!(
            snapshot_text(shim.snapshot("hidden-worker").expect("refreshed snapshot"))
                .contains(marker),
            "visible refresh should catch up the pane snapshot"
        );

        shim.kill_and_forget("hidden-worker")
            .expect("cleanup Panesmith pane");
    }

    #[test]
    fn hidden_pane_input_transaction_still_reaches_pty() {
        let mut shim = BrehonPanesmithShim::new();
        let command = "read line; printf 'ACK:%s\\n' \"$line\"; sleep 1";
        let config = test_config("sh", &["-c", command]);
        shim.spawn_pane("hidden-worker", &config, "Hidden")
            .expect("spawn Panesmith pane");

        let outcome = shim
            .send_input_transaction(
                "hidden-worker",
                InputTransaction::raw_bytes(b"work-token\r".to_vec()),
            )
            .expect("hidden pane input transaction should reach Panesmith");
        assert!(
            outcome.is_success(),
            "hidden pane input transaction failed: {outcome:?}"
        );

        let visible_panes = BTreeSet::new();
        let mut saw_output = false;
        let mut saw_ack = false;
        for _ in 0..50 {
            let events = shim.drain_events(Some(&visible_panes));
            saw_output |= events.iter().any(|event| {
                matches!(event.kind, BrehonPanesmithEventKind::Output { bytes_len } if bytes_len > 0)
            });
            assert!(
                shim.refresh_snapshot("hidden-worker")
                    .expect("refresh visible snapshot")
            );
            let text = snapshot_text(shim.snapshot("hidden-worker").expect("refreshed snapshot"));
            saw_ack = text.contains("ACK:work-token");
            if saw_ack {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            saw_output,
            "expected output from hidden pane after PTY input"
        );
        assert!(saw_ack, "hidden pane PTY did not process injected input");

        shim.kill_and_forget("hidden-worker")
            .expect("cleanup Panesmith pane");
    }

    fn snapshot_text(snapshot: &OwnedPaneSnapshot) -> String {
        snapshot
            .surface
            .rows
            .iter()
            .flat_map(|row| row.cells.iter())
            .map(|cell| cell.text.as_ref())
            .collect::<String>()
    }

    #[test]
    fn failed_spawn_leaves_mapping_empty_for_fallback() {
        let mut shim = BrehonPanesmithShim::new();
        let config = test_config("/definitely/missing/panesmith-command", &[]);

        let err = shim
            .spawn_pane("supervisor", &config, "Supervisor")
            .expect_err("missing command should fail");

        assert!(err.to_string().contains("Panesmith"));
        assert!(shim.panesmith_id_for("supervisor").is_none());
        assert!(shim.snapshot("supervisor").is_none());
    }
}
