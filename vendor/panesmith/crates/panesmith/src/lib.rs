use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PaneId(u64);

impl PaneId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl Size {
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranscriptMode {
    #[default]
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptConfig {
    pub mode: TranscriptMode,
}

impl TranscriptConfig {
    pub fn new(mode: TranscriptMode) -> Self {
        Self { mode }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneConfig {
    pub id: Option<PaneId>,
    command: String,
    args: Vec<String>,
    pub size: Size,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub transcript: TranscriptConfig,
    pub env: BTreeMap<String, String>,
}

impl PaneConfig {
    pub fn command_with_args(command: String, args: Vec<String>) -> Self {
        Self {
            id: None,
            command,
            args,
            size: Size::default(),
            cwd: None,
            title: None,
            transcript: TranscriptConfig::default(),
            env: BTreeMap::new(),
        }
    }

    pub fn with_id(mut self, id: PaneId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    pub fn with_transcript(mut self, transcript: TranscriptConfig) -> Self {
        self.transcript = transcript;
        self
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn with_env(mut self, key: String, value: String) -> Self {
        self.env.insert(key, value);
        self
    }

    pub fn program(&self) -> &str {
        &self.command
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Test-only opt-in for the compatibility shim's fake in-memory spawn path.
///
/// Production callers should not set this. Without it, `PaneManager::spawn()`
/// fails so Brehon can fall back to its legacy PTY backend until the real
/// Panesmith implementation is vendored.
pub const COMPAT_FAKE_SPAWN_ENV: &str = "__PANESMITH_COMPAT_FAKE_SPAWN";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaneManagerConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    HostRequested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneError {
    NotFound,
    InvalidConfig(String),
    Message(String),
}

impl fmt::Display for PaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "pane not found"),
            Self::InvalidConfig(message) | Self::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PaneError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneAttachError {
    message: String,
}

impl PaneAttachError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PaneAttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PaneAttachError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputKind {
    #[default]
    Bytes,
    Paste,
    Key,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCode {
    Enter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyModifiers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
}

impl KeyInput {
    pub fn new(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Self {
        Self {
            code,
            modifiers,
            kind,
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputIntent {
    InsertText(String),
    SubmitText(String),
    KeyChord(KeyInput),
    Interrupt,
    ClearInput,
    RawBytes(Vec<u8>),
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputVerification {
    None,
    EchoContains {
        needle: String,
        timeout: Duration,
    },
    EchoPrefixOrHash {
        prefix: String,
        hash: String,
        timeout: Duration,
    },
}

impl Default for InputVerification {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRetryPolicy {
    pub max_transient_retries: usize,
    pub retry_delay: Duration,
}

impl Default for InputRetryPolicy {
    fn default() -> Self {
        Self {
            max_transient_retries: 0,
            retry_delay: Duration::from_millis(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputTransaction {
    pub intent: InputIntent,
    pub verification: InputVerification,
    pub chunk_size: usize,
    pub retry: InputRetryPolicy,
}

impl InputTransaction {
    pub fn raw_bytes(bytes: Vec<u8>) -> Self {
        Self {
            intent: InputIntent::RawBytes(bytes),
            verification: InputVerification::None,
            chunk_size: usize::MAX,
            retry: InputRetryPolicy::default(),
        }
    }

    pub fn interrupt() -> Self {
        Self {
            intent: InputIntent::Interrupt,
            verification: InputVerification::None,
            chunk_size: usize::MAX,
            retry: InputRetryPolicy::default(),
        }
    }

    pub fn clear_input() -> Self {
        Self {
            intent: InputIntent::ClearInput,
            verification: InputVerification::None,
            chunk_size: usize::MAX,
            retry: InputRetryPolicy::default(),
        }
    }

    pub fn insert_text(text: impl Into<String>) -> Self {
        Self {
            intent: InputIntent::InsertText(text.into()),
            verification: InputVerification::None,
            chunk_size: usize::MAX,
            retry: InputRetryPolicy::default(),
        }
    }

    pub fn submit_text(text: impl Into<String>) -> Self {
        Self {
            intent: InputIntent::SubmitText(text.into()),
            verification: InputVerification::None,
            chunk_size: usize::MAX,
            retry: InputRetryPolicy::default(),
        }
    }

    pub fn key_chord(key: KeyInput) -> Self {
        Self {
            intent: InputIntent::KeyChord(key),
            verification: InputVerification::None,
            chunk_size: usize::MAX,
            retry: InputRetryPolicy::default(),
        }
    }

    pub fn with_verification(mut self, verification: InputVerification) -> Self {
        self.verification = verification;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputTransactionError {
    Write {
        operation: &'static str,
        bytes_attempted: usize,
        bytes_written: usize,
        message: String,
    },
    VerificationFailed {
        message: String,
    },
    ChildExited,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputOutcome {
    pub bytes_sent: usize,
    pub echoed: bool,
    pub submitted: bool,
    pub timed_out: bool,
    pub child_exited: bool,
    pub errors: Vec<InputTransactionError>,
}

impl InputOutcome {
    pub fn is_success(&self) -> bool {
        !self.timed_out && !self.child_exited && self.errors.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttachScreenPolicy {
    #[default]
    ReuseHostAlternateScreen,
    LeaveAlternateScreen,
    EnterFreshAlternateScreen,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttachDetachOptions {
    pub chord: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttachOptions {
    pub screen: AttachScreenPolicy,
    pub detach: AttachDetachOptions,
}

pub trait PaneAttachTerminal {}

pub trait PaneAttachTerminalControl {
    type Error;
    type RestoreToken;

    fn suspend_for_attach(
        &mut self,
        policy: AttachScreenPolicy,
    ) -> Result<Self::RestoreToken, Self::Error>;

    fn restore_after_attach(&mut self, token: &mut Self::RestoreToken) -> Result<(), Self::Error>;
}

pub struct StdioAttachTerminal<W> {
    _writer: W,
}

impl<W> StdioAttachTerminal<W> {
    pub fn new(writer: W) -> io::Result<Self> {
        Ok(Self { _writer: writer })
    }
}

impl<W> PaneAttachTerminal for StdioAttachTerminal<W> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneAttachReason {
    #[default]
    Detached,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneAttachOutcome {
    pub reason: PaneAttachReason,
    pub child_exit_code: Option<i32>,
    pub terminal_size: Size,
    pub restored_size: Size,
    pub remaining_input: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSpec {
    #[default]
    Reset,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg: Option<ColorSpec>,
    pub bg: Option<ColorSpec>,
    pub bold: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceCell<'a> {
    pub text: Cow<'a, str>,
    pub style: CellStyle,
}

impl<'a> Default for SurfaceCell<'a> {
    fn default() -> Self {
        Self {
            text: Cow::Borrowed(" "),
            style: CellStyle::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SurfaceRow<'a> {
    pub cells: Vec<SurfaceCell<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnedSurface {
    pub rows: Vec<SurfaceRow<'static>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnedPaneSnapshot {
    pub size: Size,
    pub surface: OwnedSurface,
}

impl OwnedPaneSnapshot {
    pub fn to_owned_snapshot(&self) -> Self {
        self.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScrollbackLine {
    pub row: SurfaceRow<'static>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnedScrollbackSnapshot {
    pub lines: Vec<ScrollbackLine>,
}

impl OwnedScrollbackSnapshot {
    pub fn to_owned_snapshot(&self) -> Self {
        self.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateChangedEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputEvent {
    pub bytes_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceChangedEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSentEvent {
    pub input_kind: InputKind,
    pub bytes_len: usize,
    pub recorded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizedEvent {
    pub size: Size,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitedEvent {
    pub code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEvent {
    pub error: PaneError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEventKind {
    Spawned(SpawnedEvent),
    StateChanged(StateChangedEvent),
    Output(OutputEvent),
    SurfaceChanged(SurfaceChangedEvent),
    InputSent(InputSentEvent),
    Resized(ResizedEvent),
    Exited(ExitedEvent),
    Error(ErrorEvent),
    AttachStarted(()),
    AttachEnded(()),
    TranscriptRotated(()),
    Overflow(()),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneEvent {
    pub pane_id: PaneId,
    pub seq: u64,
    pub kind: PaneEventKind,
}

#[derive(Debug, Clone)]
struct PaneState {
    config: PaneConfig,
    snapshot: OwnedPaneSnapshot,
    scrollback: OwnedScrollbackSnapshot,
    seq: u64,
}

pub struct PaneManager {
    panes: HashMap<PaneId, PaneState>,
    events: Vec<PaneEvent>,
    next_id: u64,
}

impl PaneManager {
    pub fn new(_: PaneManagerConfig) -> Self {
        Self {
            panes: HashMap::new(),
            events: Vec::new(),
            next_id: 1,
        }
    }

    pub fn spawn(&mut self, mut config: PaneConfig) -> Result<PaneId, PaneError> {
        validate_spawn_config(&config)?;
        if !compat_fake_spawn_enabled(&config) {
            return Err(PaneError::Message(format!(
                "compatibility Panesmith shim cannot spawn PTYs for '{}'; fall back to the legacy PTY backend until a real Panesmith backend is vendored",
                config.program()
            )));
        }

        let id = config.id.unwrap_or_else(|| {
            let next = PaneId::new(self.next_id);
            self.next_id = self.next_id.saturating_add(1);
            next
        });
        config.id = Some(id);
        let snapshot = blank_snapshot(config.size.rows, config.size.cols);
        let mut state = PaneState {
            config,
            snapshot,
            scrollback: OwnedScrollbackSnapshot::default(),
            seq: 0,
        };
        push_event(
            &mut self.events,
            id,
            &mut state.seq,
            PaneEventKind::Spawned(SpawnedEvent),
        );
        push_event(
            &mut self.events,
            id,
            &mut state.seq,
            PaneEventKind::StateChanged(StateChangedEvent),
        );
        simulate_spawn_side_effects(id, &mut state, &mut self.events);
        self.panes.insert(id, state);
        Ok(id)
    }

    pub fn send_input_transaction(
        &mut self,
        pane_id: PaneId,
        transaction: InputTransaction,
    ) -> Result<InputOutcome, PaneError> {
        let state = self.panes.get_mut(&pane_id).ok_or(PaneError::NotFound)?;
        let mut outcome = InputOutcome::default();
        let (input_kind, bytes_len, submitted, surface_changed) = match &transaction.intent {
            InputIntent::InsertText(text) => {
                append_text(&mut state.snapshot, text);
                (InputKind::Paste, text.len(), false, true)
            }
            InputIntent::SubmitText(text) => {
                append_text(&mut state.snapshot, text);
                (InputKind::Paste, text.len(), true, true)
            }
            InputIntent::KeyChord(_) => {
                append_text(&mut state.snapshot, "\n");
                (InputKind::Key, 1, false, true)
            }
            InputIntent::Interrupt => (InputKind::Key, 1, false, false),
            InputIntent::ClearInput => {
                clear_last_row(&mut state.snapshot);
                (InputKind::Key, 0, false, true)
            }
            InputIntent::RawBytes(bytes) => {
                let text = String::from_utf8_lossy(bytes);
                append_text(&mut state.snapshot, &text);
                (InputKind::Bytes, bytes.len(), false, true)
            }
        };
        outcome.bytes_sent = bytes_len;
        outcome.echoed = bytes_len > 0;
        outcome.submitted = submitted;
        push_event(
            &mut self.events,
            pane_id,
            &mut state.seq,
            PaneEventKind::InputSent(InputSentEvent {
                input_kind,
                bytes_len,
                recorded: false,
            }),
        );
        if bytes_len > 0 {
            push_event(
                &mut self.events,
                pane_id,
                &mut state.seq,
                PaneEventKind::Output(OutputEvent { bytes_len }),
            );
        }
        if surface_changed {
            push_event(
                &mut self.events,
                pane_id,
                &mut state.seq,
                PaneEventKind::SurfaceChanged(SurfaceChangedEvent),
            );
        }
        Ok(outcome)
    }

    pub fn resize(&mut self, pane_id: PaneId, size: Size) -> Result<(), PaneError> {
        let state = self.panes.get_mut(&pane_id).ok_or(PaneError::NotFound)?;
        state.config.size = size;
        state.snapshot = blank_snapshot(size.rows, size.cols);
        push_event(
            &mut self.events,
            pane_id,
            &mut state.seq,
            PaneEventKind::Resized(ResizedEvent { size }),
        );
        push_event(
            &mut self.events,
            pane_id,
            &mut state.seq,
            PaneEventKind::SurfaceChanged(SurfaceChangedEvent),
        );
        Ok(())
    }

    pub fn kill(&mut self, pane_id: PaneId, _: KillReason) -> Result<(), PaneError> {
        let Some(mut state) = self.panes.remove(&pane_id) else {
            return Err(PaneError::NotFound);
        };
        push_event(
            &mut self.events,
            pane_id,
            &mut state.seq,
            PaneEventKind::Exited(ExitedEvent { code: Some(0) }),
        );
        Ok(())
    }

    pub fn attach_blocking<Terminal, Control>(
        &mut self,
        pane_id: PaneId,
        _: AttachOptions,
        _: &mut Terminal,
        _: &mut Control,
    ) -> Result<PaneAttachOutcome, PaneAttachError>
    where
        Terminal: PaneAttachTerminal,
        Control: PaneAttachTerminalControl,
    {
        let state = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| PaneAttachError::new("pane not found"))?;
        if !compat_fake_spawn_enabled(&state.config) {
            return Err(PaneAttachError::new(
                "compatibility Panesmith shim cannot attach to a live PTY",
            ));
        }
        Ok(PaneAttachOutcome {
            reason: PaneAttachReason::Detached,
            child_exit_code: None,
            terminal_size: state.config.size,
            restored_size: state.config.size,
            remaining_input: Vec::new(),
        })
    }

    pub fn last_seq(&self, pane_id: PaneId) -> Result<u64, PaneError> {
        self.panes
            .get(&pane_id)
            .map(|state| state.seq)
            .ok_or(PaneError::NotFound)
    }

    pub fn drain_events(&mut self, output: &mut Vec<PaneEvent>) {
        output.append(&mut self.events);
    }

    pub fn snapshot(&self, pane_id: PaneId) -> Result<OwnedPaneSnapshot, PaneError> {
        self.panes
            .get(&pane_id)
            .map(|state| state.snapshot.clone())
            .ok_or(PaneError::NotFound)
    }

    pub fn scrollback(&self, pane_id: PaneId) -> Result<OwnedScrollbackSnapshot, PaneError> {
        self.panes
            .get(&pane_id)
            .map(|state| state.scrollback.clone())
            .ok_or(PaneError::NotFound)
    }
}

fn compat_fake_spawn_enabled(config: &PaneConfig) -> bool {
    config
        .env
        .get(COMPAT_FAKE_SPAWN_ENV)
        .map(String::as_str)
        .is_some_and(|value| matches!(value, "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn validate_spawn_config(config: &PaneConfig) -> Result<(), PaneError> {
    if config.program().trim().is_empty() {
        return Err(PaneError::InvalidConfig(
            "pane program must not be empty".to_string(),
        ));
    }

    if compat_fake_spawn_enabled(config)
        && config.program().contains('/')
        && !Path::new(config.program()).exists()
    {
        return Err(PaneError::InvalidConfig(format!(
            "pane program '{}' does not exist",
            config.program()
        )));
    }

    Ok(())
}

fn push_event(events: &mut Vec<PaneEvent>, pane_id: PaneId, seq: &mut u64, kind: PaneEventKind) {
    *seq = seq.saturating_add(1);
    events.push(PaneEvent {
        pane_id,
        seq: *seq,
        kind,
    });
}

fn simulate_spawn_side_effects(
    pane_id: PaneId,
    state: &mut PaneState,
    events: &mut Vec<PaneEvent>,
) {
    let Some(script) = shell_command_script(&state.config) else {
        return;
    };

    if let Some(output) = script.strip_prefix("printf ") {
        append_text(&mut state.snapshot, output);
        push_event(
            events,
            pane_id,
            &mut state.seq,
            PaneEventKind::Output(OutputEvent {
                bytes_len: output.len(),
            }),
        );
        push_event(
            events,
            pane_id,
            &mut state.seq,
            PaneEventKind::SurfaceChanged(SurfaceChangedEvent),
        );
    }

    if let Some(code) = script
        .strip_prefix("exit ")
        .and_then(|raw| raw.trim().parse::<i32>().ok())
    {
        push_event(
            events,
            pane_id,
            &mut state.seq,
            PaneEventKind::Exited(ExitedEvent { code: Some(code) }),
        );
    }
}

fn shell_command_script(config: &PaneConfig) -> Option<&str> {
    if config.program() == "sh" && config.args.len() >= 2 && config.args.first()?.as_str() == "-c" {
        return config.args.get(1).map(String::as_str);
    }
    None
}

fn blank_snapshot(rows: u16, cols: u16) -> OwnedPaneSnapshot {
    let rows = rows.max(1);
    let cols = cols.max(1);
    let col_count = cols as usize;
    let row_count = rows as usize;
    let mut surface_rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        surface_rows.push(SurfaceRow {
            cells: vec![SurfaceCell::default(); col_count],
        });
    }
    OwnedPaneSnapshot {
        size: Size::new(rows, cols),
        surface: OwnedSurface { rows: surface_rows },
    }
}

fn append_text(snapshot: &mut OwnedPaneSnapshot, text: &str) {
    if snapshot.surface.rows.is_empty() {
        snapshot.surface.rows.push(SurfaceRow::default());
    }
    let mut row = SurfaceRow::default();
    row.cells = text
        .chars()
        .map(|ch| SurfaceCell {
            text: Cow::Owned(ch.to_string()),
            style: CellStyle::default(),
        })
        .collect();
    if row.cells.is_empty() {
        row.cells.push(SurfaceCell::default());
    }
    let last = snapshot.surface.rows.len().saturating_sub(1);
    snapshot.surface.rows[last] = row;
}

fn clear_last_row(snapshot: &mut OwnedPaneSnapshot) {
    if let Some(last) = snapshot.surface.rows.last_mut() {
        *last = SurfaceRow::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_requires_fake_spawn_opt_in() {
        let mut manager = PaneManager::new(PaneManagerConfig);
        let config =
            PaneConfig::command_with_args("sh".to_string(), vec!["-c".into(), "cat".into()])
                .with_size(Size::new(8, 32));

        let err = manager
            .spawn(config)
            .expect_err("compatibility shim should force production callers to fall back");

        assert!(err
            .to_string()
            .contains("fall back to the legacy PTY backend"));
        assert!(manager.panes.is_empty());
    }

    #[test]
    fn fake_spawn_emits_script_output_event() {
        let mut manager = PaneManager::new(PaneManagerConfig);
        let config = PaneConfig::command_with_args(
            "sh".to_string(),
            vec!["-c".into(), "printf shim-output".into()],
        )
        .with_size(Size::new(4, 24))
        .with_env(COMPAT_FAKE_SPAWN_ENV.to_string(), "1".to_string());

        let pane_id = manager.spawn(config).expect("fake spawn should succeed");
        let snapshot = manager.snapshot(pane_id).expect("snapshot exists");
        let snapshot_text = snapshot
            .surface
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(snapshot_text.contains("shim-output"));

        let mut events = Vec::new();
        manager.drain_events(&mut events);
        assert!(events
            .iter()
            .any(|event| matches!(event.kind, PaneEventKind::Output(OutputEvent { bytes_len }) if bytes_len > 0)));
    }

    #[test]
    fn fake_spawn_rejects_missing_absolute_program_path() {
        let mut manager = PaneManager::new(PaneManagerConfig);
        let config = PaneConfig::command_with_args(
            "/definitely/missing/panesmith-command".to_string(),
            Vec::new(),
        )
        .with_env(COMPAT_FAKE_SPAWN_ENV.to_string(), "1".to_string());

        let err = manager
            .spawn(config)
            .expect_err("missing absolute command should be rejected");

        assert!(err.to_string().contains("does not exist"));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalViewport {
    scroll_offset: usize,
}

impl TerminalViewport {
    pub const fn scrolled(scroll_offset: usize) -> Self {
        Self { scroll_offset }
    }

    pub fn metrics(
        &self,
        _: &OwnedPaneSnapshot,
        _: Option<&OwnedScrollbackSnapshot>,
        _: usize,
    ) -> TerminalViewportMetrics {
        TerminalViewportMetrics {
            effective_scroll_offset: self.scroll_offset,
        }
    }

    pub fn scroll_up(self, amount: usize, _: TerminalViewportMetrics) -> Self {
        Self {
            scroll_offset: self.scroll_offset.saturating_add(amount),
        }
    }

    pub fn scroll_down(self, amount: usize, _: TerminalViewportMetrics) -> Self {
        Self {
            scroll_offset: self.scroll_offset.saturating_sub(amount),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalViewportMetrics {
    pub effective_scroll_offset: usize,
}

pub struct TerminalPaneWidget<'a> {
    snapshot: &'a OwnedPaneSnapshot,
    scrollback: Option<&'a OwnedScrollbackSnapshot>,
    viewport: TerminalViewport,
    focused: bool,
}

impl<'a> TerminalPaneWidget<'a> {
    pub fn new(snapshot: &'a OwnedPaneSnapshot) -> Self {
        Self {
            snapshot,
            scrollback: None,
            viewport: TerminalViewport::default(),
            focused: false,
        }
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn with_scrollback(mut self, scrollback: &'a OwnedScrollbackSnapshot) -> Self {
        self.scrollback = Some(scrollback);
        self
    }

    pub fn with_viewport(mut self, viewport: TerminalViewport) -> Self {
        self.viewport = viewport;
        self
    }
}

impl Widget for TerminalPaneWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let _ = (self.snapshot, self.scrollback, self.viewport, self.focused);
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(" ");
                }
            }
        }
    }
}
