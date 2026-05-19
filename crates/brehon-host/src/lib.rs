//! Terminal host adapter experiments.
//!
//! The embedded TUI remains the default. This crate starts with a headless
//! transcript host that consumes the same runtime event protocol, proving that
//! alternate hosts do not need a private control plane.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brehon_ports::{
    PortError, RuntimeCommandPort, RuntimeEventSink, TerminalHostAdapter, TerminalHostEventObserver,
};
use brehon_types::{
    ActivityObservedEvent, PaneExitedEvent, PaneOutputEvent, PaneSpawnedEvent,
    PaneStateChangedEvent, PromptDeliveredEvent, RuntimeActivityKind, RuntimeCommand,
    RuntimeCommandKind, RuntimeCommandResult, RuntimeCommandStatus, RuntimeEvent, RuntimeEventKind,
    RuntimeEventMeta, RuntimePaneKind, RuntimePaneState, RuntimeSource, RuntimeTerminalHostConfig,
    RuntimeTerminalHostKind, TerminalHostCapabilities, TerminalPaneHandle, TerminalPaneSpawnSpec,
    TerminalResize,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, watch};

/// Default retained transcript bytes per pane.
pub const DEFAULT_HEADLESS_TRANSCRIPT_BYTES: usize = 64 * 1024;

/// Default retained runtime events for in-memory replay.
pub const DEFAULT_HEADLESS_RECORDED_EVENTS: usize = 8 * 1024;

/// Default polling interval for terminal-host observation.
pub const DEFAULT_HOST_OBSERVATION_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Headless host configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessHostConfig {
    pub max_transcript_bytes_per_pane: usize,
    pub max_recorded_events: usize,
}

impl Default for HeadlessHostConfig {
    fn default() -> Self {
        Self {
            max_transcript_bytes_per_pane: DEFAULT_HEADLESS_TRANSCRIPT_BYTES,
            max_recorded_events: DEFAULT_HEADLESS_RECORDED_EVENTS,
        }
    }
}

/// Transcript-only terminal host.
#[derive(Debug, Default)]
pub struct HeadlessTerminalHost {
    config: HeadlessHostConfig,
    panes: RwLock<HashMap<HeadlessPaneKey, HeadlessPaneSnapshot>>,
    events: RwLock<Vec<RuntimeEvent>>,
    clock: AtomicU64,
}

/// Configuration for mapping runtime commands onto a terminal host adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalHostCommandPortConfig {
    pub default_rows: u16,
    pub default_cols: u16,
}

impl Default for TerminalHostCommandPortConfig {
    fn default() -> Self {
        Self {
            default_rows: 40,
            default_cols: 120,
        }
    }
}

/// Runtime command port backed by a terminal host adapter.
#[derive(Clone)]
pub struct TerminalHostCommandPort {
    host: Arc<dyn TerminalHostAdapter>,
    event_sink: Option<Arc<dyn RuntimeEventSink>>,
    config: TerminalHostCommandPortConfig,
}

/// Configuration for publishing observed terminal-host events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalHostObservationPumpConfig {
    pub poll_interval: Duration,
}

impl Default for TerminalHostObservationPumpConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_HOST_OBSERVATION_POLL_INTERVAL,
        }
    }
}

/// Polls a terminal-host observer and publishes events to a runtime sink.
#[derive(Clone)]
pub struct TerminalHostObservationPump {
    observer: Arc<dyn TerminalHostEventObserver>,
    event_sink: Arc<dyn RuntimeEventSink>,
    config: TerminalHostObservationPumpConfig,
}

/// Concrete terminal-host harness built from runtime config.
#[derive(Debug, Clone)]
pub enum ConfiguredTerminalHost {
    Headless(Arc<HeadlessTerminalHost>),
}

/// Operator-facing identity for a terminal-host runtime session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalHostRuntimeIdentity {
    pub session_name: Option<String>,
    pub socket_name: Option<String>,
    pub socket_dir: Option<String>,
    pub binary_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HeadlessPaneKey {
    session_id: String,
    pane_id: String,
}

/// Headless pane view built from runtime events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessPaneSnapshot {
    pub session_id: String,
    pub pane_id: String,
    pub generation: u64,
    pub state: RuntimePaneState,
    pub kind: RuntimePaneKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_bytes: Vec<u8>,
    pub transcript: String,
    pub last_event_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_reason: Option<String>,
}

impl ConfiguredTerminalHost {
    pub fn adapter(&self) -> Arc<dyn TerminalHostAdapter> {
        match self {
            Self::Headless(host) => host.clone(),
        }
    }

    pub fn observer(&self) -> Option<Arc<dyn TerminalHostEventObserver>> {
        match self {
            Self::Headless(_) => None,
        }
    }

    pub async fn shutdown(&self) -> Result<(), PortError> {
        match self {
            Self::Headless(_) => Ok(()),
        }
    }

    pub fn runtime_identity(&self, runtime_session_id: &str) -> TerminalHostRuntimeIdentity {
        match self {
            Self::Headless(_) => TerminalHostRuntimeIdentity {
                session_name: Some(runtime_session_id.to_string()),
                ..Default::default()
            },
        }
    }
}

pub fn configured_terminal_host_from_runtime_config(
    config: &RuntimeTerminalHostConfig,
    _namespace: &str,
) -> Result<Option<ConfiguredTerminalHost>, PortError> {
    match config.effective_kind() {
        RuntimeTerminalHostKind::Embedded => Ok(None),
        RuntimeTerminalHostKind::Headless => Ok(Some(ConfiguredTerminalHost::Headless(Arc::new(
            HeadlessTerminalHost::default(),
        )))),
        RuntimeTerminalHostKind::Web => Err(runtime_error(
            "runtime.terminal_host.kind web is not implemented",
        )),
        RuntimeTerminalHostKind::NativeGui => Err(runtime_error(
            "runtime.terminal_host.kind native_gui is not implemented",
        )),
    }
}

impl HeadlessTerminalHost {
    pub fn new(config: HeadlessHostConfig) -> Self {
        Self {
            config,
            panes: RwLock::new(HashMap::new()),
            events: RwLock::new(Vec::new()),
            clock: AtomicU64::new(0),
        }
    }

    /// Return runtime events emitted or consumed by this headless host.
    pub async fn events(&self) -> Vec<RuntimeEvent> {
        self.events.read().await.clone()
    }

    /// Return a sorted snapshot of all panes known to this host.
    pub async fn snapshots(&self) -> Vec<HeadlessPaneSnapshot> {
        let mut snapshots: Vec<_> = self.panes.read().await.values().cloned().collect();
        snapshots.sort_by(|a, b| {
            a.session_id
                .cmp(&b.session_id)
                .then_with(|| a.pane_id.cmp(&b.pane_id))
        });
        snapshots
    }

    /// Return one pane snapshot.
    pub async fn snapshot(&self, session_id: &str, pane_id: &str) -> Option<HeadlessPaneSnapshot> {
        self.panes
            .read()
            .await
            .get(&HeadlessPaneKey {
                session_id: session_id.to_string(),
                pane_id: pane_id.to_string(),
            })
            .cloned()
    }

    async fn record_event(&self, event: RuntimeEvent) {
        {
            let mut events = self.events.write().await;
            events.push(event.clone());
            trim_events(&mut events, self.config.max_recorded_events.max(1));
        }
        self.apply_event(event).await;
    }

    async fn record_host_events(&self, events: Vec<RuntimeEvent>) {
        if events.is_empty() {
            return;
        }
        let mut recorded = self.events.write().await;
        recorded.extend(events);
        trim_events(&mut recorded, self.config.max_recorded_events.max(1));
    }

    fn next_timestamp_ms(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn meta(
        &self,
        session_id: impl Into<String>,
        pane_id: impl Into<String>,
        generation: u64,
    ) -> RuntimeEventMeta {
        RuntimeEventMeta::new(
            session_id,
            pane_id,
            generation,
            RuntimeSource::Headless,
            self.next_timestamp_ms(),
        )
    }

    async fn apply_event(&self, event: RuntimeEvent) {
        let mut panes = self.panes.write().await;
        let key = HeadlessPaneKey {
            session_id: event.meta.session_id.clone(),
            pane_id: event.meta.pane_id.clone(),
        };

        if panes
            .get(&key)
            .is_some_and(|entry| event.meta.generation < entry.generation)
        {
            return;
        }

        let entry = panes.entry(key).or_insert_with(|| HeadlessPaneSnapshot {
            session_id: event.meta.session_id.clone(),
            pane_id: event.meta.pane_id.clone(),
            generation: event.meta.generation,
            state: RuntimePaneState::Unknown,
            kind: RuntimePaneKind::Unknown,
            title: None,
            rows: 0,
            cols: 0,
            cwd: None,
            command: Vec::new(),
            env: BTreeMap::new(),
            input_bytes: Vec::new(),
            transcript: String::new(),
            last_event_ms: event.meta.timestamp_ms,
            exit_code: None,
            exit_reason: None,
        });

        entry.generation = event.meta.generation;
        entry.last_event_ms = event.meta.timestamp_ms;

        match event.kind {
            RuntimeEventKind::PaneSpawned(PaneSpawnedEvent { kind, title }) => {
                entry.kind = kind;
                entry.title = title;
                entry.state = RuntimePaneState::Ready;
                entry.transcript.clear();
                entry.input_bytes.clear();
                entry.exit_code = None;
                entry.exit_reason = None;
            }
            RuntimeEventKind::PaneStateChanged(changed) => {
                entry.state = changed.current;
            }
            RuntimeEventKind::PaneOutput(output) => {
                append_output(entry, output, self.config.max_transcript_bytes_per_pane);
            }
            RuntimeEventKind::PaneExited(PaneExitedEvent { exit_code, reason }) => {
                entry.state = RuntimePaneState::Dead;
                entry.exit_code = exit_code;
                entry.exit_reason = reason;
            }
            RuntimeEventKind::AgentTurnStarted(_) => {
                entry.state = RuntimePaneState::Busy;
            }
            RuntimeEventKind::AgentTurnEnded(_) => {
                entry.state = RuntimePaneState::Ready;
            }
            RuntimeEventKind::PromptQueued(_)
            | RuntimeEventKind::PromptDelivered(_)
            | RuntimeEventKind::PromptRejected(_)
            | RuntimeEventKind::ActivityObserved(_)
            | RuntimeEventKind::DetectionEvent(_)
            | RuntimeEventKind::PolicyDecision(_)
            | RuntimeEventKind::WorkflowAction(_) => {}
        }
    }
}

impl TerminalHostObservationPump {
    pub fn new(
        observer: Arc<dyn TerminalHostEventObserver>,
        event_sink: Arc<dyn RuntimeEventSink>,
    ) -> Self {
        Self {
            observer,
            event_sink,
            config: TerminalHostObservationPumpConfig::default(),
        }
    }

    pub fn with_config(mut self, config: TerminalHostObservationPumpConfig) -> Self {
        self.config = config;
        self
    }

    pub async fn poll_once(&self) -> Result<usize, PortError> {
        let events = self.observer.observe_events().await?;
        let count = events.len();
        if count > 0 {
            self.event_sink.publish_batch(events).await?;
        }
        Ok(count)
    }

    pub async fn run_until_shutdown(
        &self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), PortError> {
        if *shutdown.borrow() {
            return Ok(());
        }

        let mut interval = tokio::time::interval(self.config.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                _ = interval.tick() => {
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                    self.poll_once().await?;
                }
            }
        }
    }
}

impl TerminalHostCommandPort {
    pub fn new(host: Arc<dyn TerminalHostAdapter>) -> Self {
        Self {
            host,
            event_sink: None,
            config: TerminalHostCommandPortConfig::default(),
        }
    }

    pub fn with_config(mut self, config: TerminalHostCommandPortConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_event_sink(mut self, event_sink: Arc<dyn RuntimeEventSink>) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    async fn target_handle(
        &self,
        command: &RuntimeCommand,
        operation: &str,
    ) -> Result<TerminalPaneHandle, RuntimeCommandResult> {
        let Some(pane_id) = command.target.pane_id.clone() else {
            return Err(host_rejected(
                command.command_id.clone(),
                format!("{operation} requires a pane target"),
            ));
        };
        let Some(generation) = command.target.generation else {
            return self
                .host
                .pane_handle(&command.target.session_id, &pane_id)
                .await
                .map_err(|err| host_rejected(command.command_id.clone(), err.to_string()));
        };
        Ok(TerminalPaneHandle {
            session_id: command.target.session_id.clone(),
            pane_id,
            generation,
            source: self.host.capabilities().source,
        })
    }

    async fn emit(&self, event: RuntimeEvent) -> Result<(), PortError> {
        if let Some(event_sink) = self.event_sink.as_ref() {
            event_sink.publish(event).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RuntimeCommandPort for TerminalHostCommandPort {
    async fn execute(&self, command: RuntimeCommand) -> Result<RuntimeCommandResult, PortError> {
        let command_id = command.command_id.clone();
        match command.kind.clone() {
            RuntimeCommandKind::SpawnPane {
                kind,
                pane_id,
                title,
                cwd,
                command: spawn_command,
                env,
                rows,
                cols,
            } => {
                let pane_id = pane_id
                    .or_else(|| command.target.pane_id.clone())
                    .unwrap_or_else(|| format!("pane-{command_id}"));
                let spec = TerminalPaneSpawnSpec {
                    session_id: command.target.session_id.clone(),
                    pane_id,
                    kind: kind.clone(),
                    title: title.clone(),
                    cwd,
                    command: spawn_command,
                    env,
                    rows: rows.unwrap_or(self.config.default_rows),
                    cols: cols.unwrap_or(self.config.default_cols),
                };
                let handle = match self.host.spawn_pane(spec).await {
                    Ok(handle) => handle,
                    Err(err) => return Ok(host_rejected(command_id, err.to_string())),
                };
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::PaneSpawned(PaneSpawnedEvent { kind, title }),
                ))
                .await?;
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                        previous: None,
                        current: RuntimePaneState::Ready,
                        reason: Some("terminal host spawn".to_string()),
                    }),
                ))
                .await?;
                Ok(host_applied(
                    command_id,
                    format!(
                        "spawned pane '{}' generation {}",
                        handle.pane_id, handle.generation
                    ),
                ))
            }
            RuntimeCommandKind::SendTerminalInput { bytes } => {
                let handle = match self.target_handle(&command, "terminal input").await {
                    Ok(handle) => handle,
                    Err(result) => return Ok(result),
                };
                if let Err(err) = self.host.send_input(handle.clone(), bytes.clone()).await {
                    return Ok(host_rejected(command_id, err.to_string()));
                }
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::ActivityObserved(ActivityObservedEvent {
                        kind: RuntimeActivityKind::Other {
                            name: "terminal_input".to_string(),
                        },
                        description: Some(format!("{} bytes", bytes.len())),
                    }),
                ))
                .await?;
                Ok(host_applied(command_id, "terminal input sent"))
            }
            RuntimeCommandKind::SendPrompt {
                prompt_id, text, ..
            } => {
                let handle = match self.target_handle(&command, "prompt delivery").await {
                    Ok(handle) => handle,
                    Err(result) => return Ok(result),
                };
                let input_parts = terminal_prompt_input_parts(&text);
                let input_part_count = input_parts.len();
                for (index, bytes) in input_parts.into_iter().enumerate() {
                    if let Err(err) = self.host.send_input(handle.clone(), bytes).await {
                        return Ok(host_rejected(command_id, err.to_string()));
                    }
                    if index + 1 < input_part_count {
                        tokio::time::sleep(Duration::from_millis(35)).await;
                    }
                }
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle).with_correlation_id(prompt_id.clone()),
                    RuntimeEventKind::PromptDelivered(PromptDeliveredEvent { prompt_id }),
                ))
                .await?;
                Ok(host_applied(command_id, "prompt sent to terminal host"))
            }
            RuntimeCommandKind::ResizePane { rows, cols } => {
                let handle = match self.target_handle(&command, "resize").await {
                    Ok(handle) => handle,
                    Err(result) => return Ok(result),
                };
                if let Err(err) = self
                    .host
                    .resize_pane(handle.clone(), TerminalResize { rows, cols })
                    .await
                {
                    return Ok(host_rejected(command_id, err.to_string()));
                }
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::ActivityObserved(ActivityObservedEvent {
                        kind: RuntimeActivityKind::Other {
                            name: "terminal_resize".to_string(),
                        },
                        description: Some(format!("{cols}x{rows}")),
                    }),
                ))
                .await?;
                Ok(host_applied(
                    command_id,
                    format!("pane resized to {cols}x{rows}"),
                ))
            }
            RuntimeCommandKind::ClosePane { reason } => {
                let handle = match self.target_handle(&command, "close").await {
                    Ok(handle) => handle,
                    Err(result) => return Ok(result),
                };
                if let Err(err) = self.host.close_pane(handle.clone()).await {
                    return Ok(host_rejected(command_id, err.to_string()));
                }
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::PaneExited(PaneExitedEvent {
                        exit_code: None,
                        reason: Some(reason),
                    }),
                ))
                .await?;
                Ok(host_applied(command_id, "pane closed"))
            }
            RuntimeCommandKind::ResetPane { reason }
            | RuntimeCommandKind::RecyclePane { reason } => {
                let Some(pane_id) = command.target.pane_id.clone() else {
                    return Ok(host_rejected(command_id, "respawn requires a pane target"));
                };
                let live_handle = self
                    .host
                    .pane_handle(&command.target.session_id, &pane_id)
                    .await
                    .ok();
                let previous_generation = live_handle
                    .as_ref()
                    .map(|handle| handle.generation)
                    .or(command.target.generation);
                let previous_handle = |generation| TerminalPaneHandle {
                    session_id: command.target.session_id.clone(),
                    pane_id: pane_id.clone(),
                    generation,
                    source: self.host.capabilities().source,
                };
                let spec = match self
                    .host
                    .pane_spawn_spec(&command.target.session_id, &pane_id)
                    .await
                {
                    Ok(spec) => spec,
                    Err(err) => return Ok(host_rejected(command_id, err.to_string())),
                };
                let kind = spec.kind.clone();
                let title = spec.title.clone();
                let new_handle = match self.host.spawn_pane(spec).await {
                    Ok(new_handle) => new_handle,
                    Err(err) => return Ok(host_rejected(command_id, err.to_string())),
                };
                let handle = live_handle.unwrap_or_else(|| {
                    previous_handle(
                        previous_generation
                            .unwrap_or_else(|| new_handle.generation.saturating_sub(1)),
                    )
                });
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::PaneExited(PaneExitedEvent {
                        exit_code: None,
                        reason: Some(reason.clone()),
                    }),
                ))
                .await?;
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&new_handle),
                    RuntimeEventKind::PaneSpawned(PaneSpawnedEvent { kind, title }),
                ))
                .await?;
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&new_handle),
                    RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                        previous: None,
                        current: RuntimePaneState::Ready,
                        reason: Some("terminal host respawn".to_string()),
                    }),
                ))
                .await?;
                Ok(host_applied(
                    command_id,
                    format!("pane respawned to generation {}", new_handle.generation),
                ))
            }
            RuntimeCommandKind::Interrupt { .. } => {
                let handle = match self.target_handle(&command, "interrupt").await {
                    Ok(handle) => handle,
                    Err(result) => return Ok(result),
                };
                if let Err(err) = self.host.send_input(handle.clone(), vec![3]).await {
                    return Ok(host_rejected(command_id, err.to_string()));
                }
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::ActivityObserved(ActivityObservedEvent {
                        kind: RuntimeActivityKind::Other {
                            name: "terminal_interrupt".to_string(),
                        },
                        description: None,
                    }),
                ))
                .await?;
                Ok(host_applied(command_id, "interrupt sent"))
            }
            RuntimeCommandKind::QuarantinePane { reason } => {
                let handle = match self.target_handle(&command, "quarantine").await {
                    Ok(handle) => handle,
                    Err(result) => return Ok(result),
                };
                if let Err(err) = self.host.close_pane(handle.clone()).await {
                    return Ok(host_rejected(command_id, err.to_string()));
                }
                self.emit(RuntimeEvent::new(
                    meta_for_handle(&handle),
                    RuntimeEventKind::PaneExited(PaneExitedEvent {
                        exit_code: None,
                        reason: Some(format!("quarantined: {reason}")),
                    }),
                ))
                .await?;
                Ok(host_applied(command_id, "pane quarantined"))
            }
            RuntimeCommandKind::BroadcastPrompt { .. }
            | RuntimeCommandKind::ResolveApproval { .. } => Ok(host_rejected(
                command_id,
                "runtime command is not supported by terminal host adapter",
            )),
        }
    }
}

#[async_trait]
impl RuntimeEventSink for HeadlessTerminalHost {
    async fn publish(&self, event: RuntimeEvent) -> Result<(), PortError> {
        self.record_event(event).await;
        Ok(())
    }
}

#[async_trait]
impl TerminalHostAdapter for HeadlessTerminalHost {
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
        if spec.session_id.trim().is_empty() {
            return Err(runtime_error("headless spawn requires a session id"));
        }
        if spec.pane_id.trim().is_empty() {
            return Err(runtime_error("headless spawn requires a pane id"));
        }
        if spec.rows == 0 || spec.cols == 0 {
            return Err(runtime_error("headless spawn requires non-zero dimensions"));
        }

        let key = HeadlessPaneKey {
            session_id: spec.session_id.clone(),
            pane_id: spec.pane_id.clone(),
        };
        let timestamp_ms = self.next_timestamp_ms();
        let handle;
        let previous_state;
        {
            let mut panes = self.panes.write().await;
            let previous = panes.get(&key);
            let generation = match previous {
                Some(snapshot) => snapshot
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| runtime_error("headless pane generation counter exhausted"))?,
                None => 1,
            };
            previous_state = previous.map(|snapshot| snapshot.state.clone());
            handle = TerminalPaneHandle {
                session_id: spec.session_id.clone(),
                pane_id: spec.pane_id.clone(),
                generation,
                source: RuntimeSource::Headless,
            };
            panes.insert(
                key,
                HeadlessPaneSnapshot {
                    session_id: spec.session_id.clone(),
                    pane_id: spec.pane_id.clone(),
                    generation,
                    state: RuntimePaneState::Ready,
                    kind: spec.kind.clone(),
                    title: spec.title.clone(),
                    rows: spec.rows,
                    cols: spec.cols,
                    cwd: spec.cwd,
                    command: spec.command,
                    env: spec.env,
                    input_bytes: Vec::new(),
                    transcript: String::new(),
                    last_event_ms: timestamp_ms,
                    exit_code: None,
                    exit_reason: None,
                },
            );
        }

        self.record_host_events(vec![
            RuntimeEvent::new(
                RuntimeEventMeta::new(
                    handle.session_id.clone(),
                    handle.pane_id.clone(),
                    handle.generation,
                    RuntimeSource::Headless,
                    timestamp_ms,
                ),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: spec.kind,
                    title: spec.title,
                }),
            ),
            RuntimeEvent::new(
                self.meta(
                    handle.session_id.clone(),
                    handle.pane_id.clone(),
                    handle.generation,
                ),
                RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                    previous: previous_state,
                    current: RuntimePaneState::Ready,
                    reason: Some("headless spawn".to_string()),
                }),
            ),
        ])
        .await;

        Ok(handle)
    }

    async fn pane_handle(
        &self,
        session_id: &str,
        pane_id: &str,
    ) -> Result<TerminalPaneHandle, PortError> {
        let key = HeadlessPaneKey {
            session_id: session_id.to_string(),
            pane_id: pane_id.to_string(),
        };
        let panes = self.panes.read().await;
        let snapshot = panes
            .get(&key)
            .ok_or_else(|| runtime_error("unknown headless pane handle"))?;
        if snapshot.state == RuntimePaneState::Dead {
            return Err(runtime_error("headless pane is closed"));
        }
        Ok(TerminalPaneHandle {
            session_id: snapshot.session_id.clone(),
            pane_id: snapshot.pane_id.clone(),
            generation: snapshot.generation,
            source: RuntimeSource::Headless,
        })
    }

    async fn pane_spawn_spec(
        &self,
        session_id: &str,
        pane_id: &str,
    ) -> Result<TerminalPaneSpawnSpec, PortError> {
        let key = HeadlessPaneKey {
            session_id: session_id.to_string(),
            pane_id: pane_id.to_string(),
        };
        let panes = self.panes.read().await;
        let snapshot = panes
            .get(&key)
            .ok_or_else(|| runtime_error("unknown headless pane handle"))?;
        Ok(TerminalPaneSpawnSpec {
            session_id: snapshot.session_id.clone(),
            pane_id: snapshot.pane_id.clone(),
            kind: snapshot.kind.clone(),
            title: snapshot.title.clone(),
            cwd: snapshot.cwd.clone(),
            command: snapshot.command.clone(),
            env: snapshot.env.clone(),
            rows: snapshot.rows,
            cols: snapshot.cols,
        })
    }

    async fn close_pane(&self, handle: TerminalPaneHandle) -> Result<(), PortError> {
        validate_headless_handle(&handle)?;
        let mut event = None;
        {
            let mut panes = self.panes.write().await;
            let snapshot = pane_for_handle_mut(&mut panes, &handle)?;
            if snapshot.state != RuntimePaneState::Dead {
                snapshot.state = RuntimePaneState::Dead;
                snapshot.exit_code = None;
                snapshot.exit_reason = Some("closed".to_string());
                snapshot.last_event_ms = self.next_timestamp_ms();
                event = Some(RuntimeEvent::new(
                    RuntimeEventMeta::new(
                        handle.session_id.clone(),
                        handle.pane_id.clone(),
                        handle.generation,
                        RuntimeSource::Headless,
                        snapshot.last_event_ms,
                    ),
                    RuntimeEventKind::PaneExited(PaneExitedEvent {
                        exit_code: None,
                        reason: Some("closed".to_string()),
                    }),
                ));
            }
        }

        if let Some(event) = event {
            self.record_host_events(vec![event]).await;
        }
        Ok(())
    }

    async fn send_input(
        &self,
        handle: TerminalPaneHandle,
        bytes: Vec<u8>,
    ) -> Result<(), PortError> {
        validate_headless_handle(&handle)?;
        let timestamp_ms;
        {
            let mut panes = self.panes.write().await;
            let snapshot = pane_for_open_handle_mut(&mut panes, &handle)?;
            snapshot.input_bytes.extend(bytes.iter());
            trim_bytes(
                &mut snapshot.input_bytes,
                self.config.max_transcript_bytes_per_pane.max(1),
            );
            timestamp_ms = self.next_timestamp_ms();
            snapshot.last_event_ms = timestamp_ms;
        }

        self.record_host_events(vec![RuntimeEvent::new(
            RuntimeEventMeta::new(
                handle.session_id,
                handle.pane_id,
                handle.generation,
                RuntimeSource::Headless,
                timestamp_ms,
            ),
            RuntimeEventKind::ActivityObserved(ActivityObservedEvent {
                kind: RuntimeActivityKind::Other {
                    name: "terminal_input".to_string(),
                },
                description: Some(format!("{} bytes", bytes.len())),
            }),
        )])
        .await;
        Ok(())
    }

    async fn resize_pane(
        &self,
        handle: TerminalPaneHandle,
        size: TerminalResize,
    ) -> Result<(), PortError> {
        validate_headless_handle(&handle)?;
        if size.rows == 0 || size.cols == 0 {
            return Err(runtime_error(
                "headless resize requires non-zero dimensions",
            ));
        }

        let timestamp_ms;
        {
            let mut panes = self.panes.write().await;
            let snapshot = pane_for_open_handle_mut(&mut panes, &handle)?;
            snapshot.rows = size.rows;
            snapshot.cols = size.cols;
            timestamp_ms = self.next_timestamp_ms();
            snapshot.last_event_ms = timestamp_ms;
        }

        self.record_host_events(vec![RuntimeEvent::new(
            RuntimeEventMeta::new(
                handle.session_id,
                handle.pane_id,
                handle.generation,
                RuntimeSource::Headless,
                timestamp_ms,
            ),
            RuntimeEventKind::ActivityObserved(ActivityObservedEvent {
                kind: RuntimeActivityKind::Other {
                    name: "terminal_resize".to_string(),
                },
                description: Some(format!("{}x{}", size.cols, size.rows)),
            }),
        )])
        .await;
        Ok(())
    }
}

fn validate_headless_handle(handle: &TerminalPaneHandle) -> Result<(), PortError> {
    if handle.source != RuntimeSource::Headless {
        return Err(runtime_error("terminal handle belongs to a different host"));
    }
    Ok(())
}

fn pane_for_handle_mut<'a>(
    panes: &'a mut HashMap<HeadlessPaneKey, HeadlessPaneSnapshot>,
    handle: &TerminalPaneHandle,
) -> Result<&'a mut HeadlessPaneSnapshot, PortError> {
    let key = HeadlessPaneKey {
        session_id: handle.session_id.clone(),
        pane_id: handle.pane_id.clone(),
    };
    let snapshot = panes
        .get_mut(&key)
        .ok_or_else(|| runtime_error("unknown headless pane handle"))?;
    if snapshot.generation != handle.generation {
        return Err(runtime_error(format!(
            "stale headless pane handle: requested generation {}, current generation {}",
            handle.generation, snapshot.generation
        )));
    }
    Ok(snapshot)
}

fn pane_for_open_handle_mut<'a>(
    panes: &'a mut HashMap<HeadlessPaneKey, HeadlessPaneSnapshot>,
    handle: &TerminalPaneHandle,
) -> Result<&'a mut HeadlessPaneSnapshot, PortError> {
    let snapshot = pane_for_handle_mut(panes, handle)?;
    if snapshot.state == RuntimePaneState::Dead {
        return Err(runtime_error("headless pane is closed"));
    }
    Ok(snapshot)
}

pub(crate) fn runtime_error(message: impl Into<String>) -> PortError {
    PortError::Runtime(message.into())
}

fn host_applied(command_id: String, message: impl Into<String>) -> RuntimeCommandResult {
    RuntimeCommandResult {
        command_id,
        status: RuntimeCommandStatus::Applied,
        message: Some(message.into()),
    }
}

fn host_rejected(command_id: String, message: impl Into<String>) -> RuntimeCommandResult {
    RuntimeCommandResult {
        command_id,
        status: RuntimeCommandStatus::Rejected,
        message: Some(message.into()),
    }
}

fn meta_for_handle(handle: &TerminalPaneHandle) -> RuntimeEventMeta {
    RuntimeEventMeta::new(
        handle.session_id.clone(),
        handle.pane_id.clone(),
        handle.generation,
        handle.source.clone(),
        unix_timestamp_ms(),
    )
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn terminal_prompt_input_parts(text: &str) -> Vec<Vec<u8>> {
    let text = text.trim();
    vec![vec![0x15], text.as_bytes().to_vec(), vec![b'\r']]
}

fn append_output(
    entry: &mut HeadlessPaneSnapshot,
    output: PaneOutputEvent,
    max_transcript_bytes: usize,
) {
    if let Some(text) = output.text {
        entry.transcript.push_str(&text);
    } else {
        entry
            .transcript
            .push_str(&String::from_utf8_lossy(&output.bytes));
    }
    trim_transcript(&mut entry.transcript, max_transcript_bytes.max(1));
}

fn trim_transcript(transcript: &mut String, max_bytes: usize) {
    if transcript.len() <= max_bytes {
        return;
    }
    let min_start = transcript.len() - max_bytes;
    let start = transcript
        .char_indices()
        .map(|(idx, _)| idx)
        .find(|idx| *idx >= min_start)
        .unwrap_or(transcript.len());
    transcript.drain(..start);
}

fn trim_bytes(bytes: &mut Vec<u8>, max_bytes: usize) {
    if bytes.len() <= max_bytes {
        return;
    }
    let start = bytes.len() - max_bytes;
    bytes.drain(..start);
}

fn trim_events(events: &mut Vec<RuntimeEvent>, max_events: usize) {
    if events.len() <= max_events {
        return;
    }
    let start = events.len() - max_events;
    events.drain(..start);
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_ports::{RuntimeCommandPort, RuntimeEventSink};
    use brehon_types::{
        AgentTurnEvent, PromptDeliveryMode, RuntimeCommandTarget, RuntimeEventMeta,
        RuntimePolicyContext, RuntimeSource,
    };
    use async_trait::async_trait;

    fn spawn_spec() -> TerminalPaneSpawnSpec {
        TerminalPaneSpawnSpec {
            session_id: "session".to_string(),
            pane_id: "pane".to_string(),
            kind: RuntimePaneKind::Worker,
            title: Some("worker".to_string()),
            cwd: Some("/tmp".to_string()),
            command: vec!["agent".to_string(), "run".to_string()],
            env: BTreeMap::from([("BREHON_TEST".to_string(), "1".to_string())]),
            rows: 40,
            cols: 120,
        }
    }

    fn event(pane_id: &str, generation: u64, kind: RuntimeEventKind) -> RuntimeEvent {
        RuntimeEvent::new(
            RuntimeEventMeta::new("session", pane_id, generation, RuntimeSource::Headless, 1),
            kind,
        )
    }

    fn runtime_command(
        command_id: &str,
        generation: Option<u64>,
        kind: RuntimeCommandKind,
    ) -> RuntimeCommand {
        RuntimeCommand {
            command_id: command_id.to_string(),
            target: RuntimeCommandTarget {
                session_id: "session".to_string(),
                pane_id: Some("pane".to_string()),
                generation,
            },
            issued_at_ms: 1,
            kind,
        }
    }

    async fn publish_new_host_events(
        host: &HeadlessTerminalHost,
        daemon: &brehon_daemon::RuntimeDaemon,
        cursor: &mut usize,
    ) {
        let events = host.events().await;
        for event in events.iter().skip(*cursor).cloned() {
            daemon.publish(event).await.expect("publish host event");
        }
        *cursor = events.len();
    }

    #[derive(Debug)]
    struct RecordingObserver {
        batches: tokio::sync::Mutex<Vec<Vec<RuntimeEvent>>>,
    }

    impl RecordingObserver {
        fn new(batches: Vec<Vec<RuntimeEvent>>) -> Self {
            Self {
                batches: tokio::sync::Mutex::new(batches),
            }
        }
    }

    #[async_trait]
    impl TerminalHostEventObserver for RecordingObserver {
        async fn observe_events(&self) -> Result<Vec<RuntimeEvent>, PortError> {
            let mut batches = self.batches.lock().await;
            if batches.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(batches.remove(0))
            }
        }
    }

    #[derive(Debug, Default)]
    struct RecordingSink {
        events: tokio::sync::Mutex<Vec<RuntimeEvent>>,
    }

    #[async_trait]
    impl RuntimeEventSink for RecordingSink {
        async fn publish(&self, event: RuntimeEvent) -> Result<(), PortError> {
            self.events.lock().await.push(event);
            Ok(())
        }
    }

    #[tokio::test]
    async fn observation_pump_publishes_observed_events() {
        let first = event(
            "pane",
            1,
            RuntimeEventKind::PaneOutput(PaneOutputEvent {
                bytes: b"hello".to_vec(),
                text: Some("hello".to_string()),
            }),
        );
        let second = event(
            "pane",
            1,
            RuntimeEventKind::PaneExited(PaneExitedEvent {
                exit_code: Some(0),
                reason: Some("done".to_string()),
            }),
        );
        let observer = Arc::new(RecordingObserver::new(vec![vec![
            first.clone(),
            second.clone(),
        ]]));
        let sink = Arc::new(RecordingSink::default());
        let pump = TerminalHostObservationPump::new(observer, sink.clone());

        assert_eq!(pump.poll_once().await.expect("poll"), 2);
        assert_eq!(pump.poll_once().await.expect("poll empty"), 0);
        assert_eq!(
            sink.events.lock().await.clone(),
            vec![first, second],
            "pump should publish observed host events in order"
        );
    }

    #[test]
    fn runtime_config_factory_keeps_embedded_external_boundary_explicit() {
        let embedded = configured_terminal_host_from_runtime_config(
            &RuntimeTerminalHostConfig::default(),
            "session",
        )
        .expect("embedded config");
        assert!(embedded.is_none());

        let mut headless = RuntimeTerminalHostConfig {
            kind: Some(RuntimeTerminalHostKind::Headless),
            ..RuntimeTerminalHostConfig::default()
        };
        let configured =
            configured_terminal_host_from_runtime_config(&headless, "session").expect("headless");
        let configured = configured.expect("headless host");
        assert_eq!(
            configured.adapter().capabilities().source,
            RuntimeSource::Headless
        );
        assert!(configured.observer().is_none());

        headless.kind = Some(RuntimeTerminalHostKind::Web);
        assert!(
            configured_terminal_host_from_runtime_config(&headless, "session")
                .expect_err("web host should not be implemented")
                .to_string()
                .contains("not implemented")
        );
    }

    #[test]
    fn terminal_prompt_input_submits_as_separate_events() {
        assert_eq!(
            terminal_prompt_input_parts(" do the thing\n"),
            vec![vec![0x15], b"do the thing".to_vec(), b"\r".to_vec()]
        );
    }

    #[test]
    fn runtime_config_factory_builds_headless_host() {
        let configured = configured_terminal_host_from_runtime_config(
            &RuntimeTerminalHostConfig {
                kind: Some(RuntimeTerminalHostKind::Headless),
                ..RuntimeTerminalHostConfig::default()
            },
            "session",
        )
        .expect("headless host should be accepted")
        .expect("headless host should be built");

        assert!(matches!(configured, ConfiguredTerminalHost::Headless(_)));
    }

    #[tokio::test]
    async fn headless_host_tracks_spawn_output_and_exit() {
        let host = HeadlessTerminalHost::default();
        host.publish(event(
            "pane",
            1,
            RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                kind: RuntimePaneKind::Worker,
                title: Some("worker".to_string()),
            }),
        ))
        .await
        .expect("publish spawn");
        host.publish(event(
            "pane",
            1,
            RuntimeEventKind::PaneOutput(PaneOutputEvent {
                bytes: b"hello".to_vec(),
                text: None,
            }),
        ))
        .await
        .expect("publish output");
        host.publish(event(
            "pane",
            1,
            RuntimeEventKind::PaneExited(PaneExitedEvent {
                exit_code: Some(0),
                reason: Some("done".to_string()),
            }),
        ))
        .await
        .expect("publish exit");

        let snapshot = host
            .snapshot("session", "pane")
            .await
            .expect("pane snapshot");
        assert_eq!(snapshot.kind, RuntimePaneKind::Worker);
        assert_eq!(snapshot.transcript, "hello");
        assert_eq!(snapshot.state, RuntimePaneState::Dead);
        assert_eq!(snapshot.exit_code, Some(0));
    }

    #[tokio::test]
    async fn headless_host_updates_turn_state_from_runtime_events() {
        let host = HeadlessTerminalHost::default();
        host.publish(event(
            "pane",
            2,
            RuntimeEventKind::AgentTurnStarted(AgentTurnEvent {
                prompt_id: Some("p".to_string()),
                reason: None,
            }),
        ))
        .await
        .expect("publish turn start");
        assert_eq!(
            host.snapshot("session", "pane").await.expect("pane").state,
            RuntimePaneState::Busy
        );

        host.publish(event(
            "pane",
            2,
            RuntimeEventKind::AgentTurnEnded(AgentTurnEvent {
                prompt_id: Some("p".to_string()),
                reason: None,
            }),
        ))
        .await
        .expect("publish turn end");
        assert_eq!(
            host.snapshot("session", "pane").await.expect("pane").state,
            RuntimePaneState::Ready
        );
    }

    #[tokio::test]
    async fn headless_host_bounds_transcript_without_splitting_utf8() {
        let host = HeadlessTerminalHost::new(HeadlessHostConfig {
            max_transcript_bytes_per_pane: 5,
            ..HeadlessHostConfig::default()
        });
        host.publish(event(
            "pane",
            1,
            RuntimeEventKind::PaneOutput(PaneOutputEvent {
                bytes: Vec::new(),
                text: Some("abcéfg".to_string()),
            }),
        ))
        .await
        .expect("publish output");

        let snapshot = host.snapshot("session", "pane").await.expect("pane");
        assert!(snapshot.transcript.len() <= 5);
        assert!(snapshot.transcript.is_char_boundary(0));
    }

    #[tokio::test]
    async fn headless_host_bounds_recorded_events() {
        let host = HeadlessTerminalHost::new(HeadlessHostConfig {
            max_recorded_events: 2,
            ..HeadlessHostConfig::default()
        });

        for pane_id in ["one", "two", "three"] {
            host.publish(event(
                pane_id,
                1,
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: None,
                }),
            ))
            .await
            .expect("publish spawn");
        }

        let events = host.events().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].meta.pane_id, "two");
        assert_eq!(events[1].meta.pane_id, "three");
    }

    #[tokio::test]
    async fn headless_adapter_reports_fixed_capabilities() {
        let host = HeadlessTerminalHost::default();

        assert_eq!(
            host.capabilities(),
            TerminalHostCapabilities {
                source: RuntimeSource::Headless,
                interactive_pty: false,
                scrollback: true,
                structured_activity: true,
                absolute_resize: true,
                out_of_process_lifecycle: false,
                replay: true,
            }
        );
    }

    #[tokio::test]
    async fn headless_adapter_spawns_records_input_resizes_and_closes() {
        let host = HeadlessTerminalHost::default();
        let handle = host.spawn_pane(spawn_spec()).await.expect("spawn pane");

        assert_eq!(handle.generation, 1);
        assert_eq!(handle.source, RuntimeSource::Headless);
        let snapshot = host
            .snapshot("session", "pane")
            .await
            .expect("spawned pane");
        assert_eq!(snapshot.state, RuntimePaneState::Ready);
        assert_eq!(snapshot.rows, 40);
        assert_eq!(snapshot.cols, 120);
        assert_eq!(snapshot.cwd.as_deref(), Some("/tmp"));
        assert_eq!(snapshot.command, ["agent", "run"]);
        assert_eq!(
            snapshot.env.get("BREHON_TEST").map(String::as_str),
            Some("1")
        );

        host.send_input(handle.clone(), b"hello".to_vec())
            .await
            .expect("send input");
        host.resize_pane(
            handle.clone(),
            TerminalResize {
                rows: 50,
                cols: 160,
            },
        )
        .await
        .expect("resize pane");

        let snapshot = host
            .snapshot("session", "pane")
            .await
            .expect("updated pane");
        assert_eq!(snapshot.input_bytes, b"hello");
        assert_eq!(snapshot.rows, 50);
        assert_eq!(snapshot.cols, 160);

        host.close_pane(handle.clone()).await.expect("close pane");
        let snapshot = host.snapshot("session", "pane").await.expect("closed pane");
        assert_eq!(snapshot.state, RuntimePaneState::Dead);
        assert_eq!(snapshot.exit_reason.as_deref(), Some("closed"));
        assert!(host.send_input(handle, b"!".to_vec()).await.is_err());

        let events = host.events().await;
        assert_eq!(events.len(), 5);
        assert!(matches!(events[0].kind, RuntimeEventKind::PaneSpawned(_)));
        assert!(matches!(
            events[1].kind,
            RuntimeEventKind::PaneStateChanged(_)
        ));
        assert!(matches!(
            events[2].kind,
            RuntimeEventKind::ActivityObserved(_)
        ));
        assert!(matches!(
            events[3].kind,
            RuntimeEventKind::ActivityObserved(_)
        ));
        assert!(matches!(events[4].kind, RuntimeEventKind::PaneExited(_)));
    }

    #[tokio::test]
    async fn headless_adapter_respawn_bumps_generation_and_rejects_stale_handles() {
        let host = HeadlessTerminalHost::default();
        let first = host.spawn_pane(spawn_spec()).await.expect("first spawn");
        host.send_input(first.clone(), b"old".to_vec())
            .await
            .expect("first input");

        let second = host.spawn_pane(spawn_spec()).await.expect("second spawn");

        assert_eq!(second.generation, 2);
        assert!(
            host.send_input(first.clone(), b"stale".to_vec())
                .await
                .is_err()
        );
        assert!(
            host.resize_pane(first, TerminalResize { rows: 24, cols: 80 },)
                .await
                .is_err()
        );

        let snapshot = host
            .snapshot("session", "pane")
            .await
            .expect("respawned pane");
        assert_eq!(snapshot.generation, 2);
        assert!(snapshot.input_bytes.is_empty());
    }

    #[tokio::test]
    async fn headless_host_ignores_stale_published_events() {
        let host = HeadlessTerminalHost::default();
        let first = host.spawn_pane(spawn_spec()).await.expect("first spawn");
        assert_eq!(first.generation, 1);
        let second = host.spawn_pane(spawn_spec()).await.expect("second spawn");
        assert_eq!(second.generation, 2);

        host.publish(event(
            "pane",
            1,
            RuntimeEventKind::PaneOutput(PaneOutputEvent {
                bytes: b"stale".to_vec(),
                text: None,
            }),
        ))
        .await
        .expect("publish stale output");

        let snapshot = host.snapshot("session", "pane").await.expect("pane");
        assert_eq!(snapshot.generation, 2);
        assert!(snapshot.transcript.is_empty());
        assert!(matches!(
            host.events().await.last().map(|event| &event.kind),
            Some(RuntimeEventKind::PaneOutput(_))
        ));
    }

    #[tokio::test]
    async fn headless_adapter_rejects_mutation_after_exit_event() {
        let host = HeadlessTerminalHost::default();
        let handle = host.spawn_pane(spawn_spec()).await.expect("spawn");

        host.publish(event(
            "pane",
            1,
            RuntimeEventKind::PaneExited(PaneExitedEvent {
                exit_code: Some(137),
                reason: Some("host exit".to_string()),
            }),
        ))
        .await
        .expect("publish exit");

        assert_eq!(
            host.snapshot("session", "pane").await.expect("pane").state,
            RuntimePaneState::Dead
        );
        assert!(
            host.send_input(handle.clone(), b"after".to_vec())
                .await
                .is_err()
        );
        assert!(
            host.resize_pane(handle, TerminalResize { rows: 24, cols: 80 },)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn headless_host_replays_recorded_events_into_fresh_host() {
        let host = HeadlessTerminalHost::default();
        let handle = host.spawn_pane(spawn_spec()).await.expect("spawn");
        host.publish(event(
            "pane",
            handle.generation,
            RuntimeEventKind::PaneOutput(PaneOutputEvent {
                bytes: b"line one\n".to_vec(),
                text: None,
            }),
        ))
        .await
        .expect("publish output");
        host.close_pane(handle).await.expect("close");

        let replayed = HeadlessTerminalHost::default();
        replayed
            .publish_batch(host.events().await)
            .await
            .expect("replay events");
        let snapshot = replayed
            .snapshot("session", "pane")
            .await
            .expect("replayed pane");

        assert_eq!(snapshot.state, RuntimePaneState::Dead);
        assert_eq!(snapshot.kind, RuntimePaneKind::Worker);
        assert_eq!(snapshot.title.as_deref(), Some("worker"));
        assert_eq!(snapshot.transcript, "line one\n");
        assert_eq!(snapshot.exit_reason.as_deref(), Some("closed"));
    }

    #[tokio::test]
    async fn terminal_host_command_port_routes_daemon_commands_to_headless_host() {
        let host = Arc::new(HeadlessTerminalHost::default());
        let adapter: Arc<dyn TerminalHostAdapter> = host.clone();
        let command_port: Arc<dyn RuntimeCommandPort> =
            Arc::new(TerminalHostCommandPort::new(adapter));
        let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port),
            ..brehon_daemon::RuntimeDaemonConfig::default()
        });
        let mut event_cursor = 0usize;

        let result = daemon
            .route_command(
                runtime_command(
                    "spawn",
                    None,
                    RuntimeCommandKind::SpawnPane {
                        kind: RuntimePaneKind::Worker,
                        pane_id: Some("pane".to_string()),
                        title: Some("worker".to_string()),
                        cwd: Some("/tmp".to_string()),
                        command: Vec::new(),
                        env: BTreeMap::new(),
                        rows: Some(30),
                        cols: Some(100),
                    },
                ),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route spawn");
        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        publish_new_host_events(&host, &daemon, &mut event_cursor).await;
        let snapshot = host.snapshot("session", "pane").await.expect("pane");
        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.rows, 30);
        assert_eq!(snapshot.cols, 100);
        assert_eq!(
            daemon.pane_registry_snapshot().await.panes[0].state,
            RuntimePaneState::Ready
        );

        let result = daemon
            .route_command(
                runtime_command(
                    "input",
                    Some(1),
                    RuntimeCommandKind::SendTerminalInput {
                        bytes: b"hello".to_vec(),
                    },
                ),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route input");
        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        assert_eq!(
            host.snapshot("session", "pane")
                .await
                .expect("pane")
                .input_bytes,
            b"hello"
        );

        let result = daemon
            .route_command(
                runtime_command(
                    "prompt",
                    None,
                    RuntimeCommandKind::SendPrompt {
                        prompt_id: "prompt-1".to_string(),
                        text: " do the thing\n".to_string(),
                        from: None,
                        delivery: PromptDeliveryMode::Enqueue,
                    },
                ),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route prompt");
        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        assert_eq!(
            host.snapshot("session", "pane")
                .await
                .expect("pane")
                .input_bytes,
            b"hello\x15do the thing\r"
        );

        let result = daemon
            .route_command(
                runtime_command(
                    "recycle",
                    None,
                    RuntimeCommandKind::RecyclePane {
                        reason: "refresh".to_string(),
                    },
                ),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route recycle");
        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        publish_new_host_events(&host, &daemon, &mut event_cursor).await;
        let snapshot = host.snapshot("session", "pane").await.expect("pane");
        assert_eq!(snapshot.generation, 2);
        assert_eq!(snapshot.input_bytes, b"");
        assert_eq!(daemon.pane_registry_snapshot().await.panes[0].generation, 2);

        let result = daemon
            .route_command(
                runtime_command(
                    "resize",
                    Some(2),
                    RuntimeCommandKind::ResizePane {
                        rows: 50,
                        cols: 160,
                    },
                ),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route resize");
        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        let snapshot = host.snapshot("session", "pane").await.expect("pane");
        assert_eq!(snapshot.rows, 50);
        assert_eq!(snapshot.cols, 160);

        let result = daemon
            .route_command(
                runtime_command(
                    "close",
                    Some(2),
                    RuntimeCommandKind::ClosePane {
                        reason: "done".to_string(),
                    },
                ),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route close");
        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        publish_new_host_events(&host, &daemon, &mut event_cursor).await;
        assert_eq!(
            host.snapshot("session", "pane").await.expect("pane").state,
            RuntimePaneState::Dead
        );
        assert_eq!(
            daemon.pane_registry_snapshot().await.panes[0].state,
            RuntimePaneState::Dead
        );
    }
}
