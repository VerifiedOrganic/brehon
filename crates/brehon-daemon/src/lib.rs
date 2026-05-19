//! Runtime daemon coordination plane.
//!
//! The daemon starts as a sidecar boundary: it fans out runtime events, keeps a
//! pane registry snapshot, audits policy decisions, and routes commands through
//! policy without taking PTY ownership away from the mux.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use brehon_detect::{DetectionLoopStats, PatternDetectionEngine, run_detection_loop};
use brehon_ports::{
    DetectionEngine, PolicyGate, PortError, RuntimeCommandPort, RuntimeCommandRouter,
    RuntimeEventSink, RuntimeEventStream,
};
use brehon_runtime::{DEFAULT_RUNTIME_EVENT_CAPACITY, RuntimeEventBus, RuntimeEventReceiver};
use brehon_types::{
    PaneExitedEvent, PaneSpawnedEvent, PolicyDecisionEvent, RuntimeCommand, RuntimeCommandResult,
    RuntimeCommandStatus, RuntimeEvent, RuntimeEventKind, RuntimeEventMeta, RuntimeOperation,
    RuntimePaneKind, RuntimePaneState, RuntimePolicyContext, RuntimePolicyDecision,
    RuntimePolicyRequest, RuntimeSource, RuntimeTerminalHostKind, RuntimeTerminalHostPaneOwnership,
    TerminalHostCapabilities,
};
use brehon_workflow::{WorkflowEngine, WorkflowLoopStats};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

const APPROVAL_STORE_SCHEMA_VERSION: u32 = 1;
const MAX_COMMAND_INBOX_REQUEST_ID_LEN: usize = 128;
/// Daemon construction options.
#[derive(Clone, Default)]
pub struct RuntimeDaemonConfig {
    /// Runtime event buffer capacity per subscriber.
    pub event_capacity: Option<usize>,
    /// Optional policy gate used before command execution.
    pub policy_gate: Option<Arc<dyn PolicyGate>>,
    /// Optional command executor. If omitted, allowed commands are accepted for
    /// audit only and are not applied.
    pub command_port: Option<Arc<dyn RuntimeCommandPort>>,
    /// Optional append-only JSONL audit log for every daemon event.
    pub audit_log_path: Option<PathBuf>,
    /// Optional JSON store for pending approvals.
    pub approval_store_path: Option<PathBuf>,
    /// Optional session id used to reject stale persisted approvals.
    pub approval_store_session_id: Option<String>,
    /// Optional terminal-host mode advertised by the launcher.
    pub terminal_host: Option<RuntimeTerminalHostStatus>,
}

/// Runtime coordination sidecar.
#[derive(Clone)]
pub struct RuntimeDaemon {
    inner: Arc<RuntimeDaemonInner>,
}

/// Background sidecar loops for advisory detection and dry-run workflows.
pub struct RuntimeSidecar {
    daemon: RuntimeDaemon,
    detection_task: JoinHandle<DetectionLoopStats>,
    workflow_task: JoinHandle<WorkflowLoopStats>,
    status: RuntimeSidecarStatusHandle,
}

#[derive(Debug)]
struct RuntimeSidecarState {
    detection_running: AtomicBool,
    workflow_running: AtomicBool,
}

/// Cloneable handle for observing sidecar loop liveness.
#[derive(Clone, Debug)]
pub struct RuntimeSidecarStatusHandle {
    inner: Arc<RuntimeSidecarState>,
}

/// Point-in-time sidecar loop liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSidecarStatus {
    pub detection_running: bool,
    pub workflow_running: bool,
}

/// Terminal host mode advertised by the launcher in daemon status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeTerminalHostStatus {
    pub kind: RuntimeTerminalHostKind,
    pub experimental: bool,
    pub observation_running: bool,
    #[serde(default)]
    pub command_routing: RuntimeTerminalHostCommandRouting,
    #[serde(default)]
    pub pane_ownership: RuntimeTerminalHostPaneOwnership,
    #[serde(default)]
    pub agent_factory: RuntimeTerminalHostAgentFactoryRouting,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<TerminalHostCapabilities>,
    #[serde(default)]
    pub promotion_readiness: RuntimeTerminalHostPromotionReadiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<RuntimeTerminalHostDiagnostic>,
}

/// Operator-facing runtime diagnostic for a terminal-host configuration or
/// observed host state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeTerminalHostDiagnostic {
    pub severity: RuntimeTerminalHostDiagnosticSeverity,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTerminalHostDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// Derived readiness report for promoting a terminal host to own real agent
/// panes in `brehon run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeTerminalHostPromotionReadiness {
    pub ready: bool,
    #[serde(default)]
    pub blockers: Vec<String>,
}

/// Command-plane owner for daemon-routed runtime commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTerminalHostCommandRouting {
    #[default]
    Mux,
    TerminalHost,
}

/// Owner of worker/reviewer/supervisor pane creation in `brehon run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTerminalHostAgentFactoryRouting {
    #[default]
    Mux,
    TerminalHost,
}

pub fn terminal_host_promotion_readiness(
    host: Option<&RuntimeTerminalHostStatus>,
) -> RuntimeTerminalHostPromotionReadiness {
    let Some(host) = host else {
        return RuntimeTerminalHostPromotionReadiness {
            ready: false,
            blockers: vec!["terminal host status is missing".to_string()],
        };
    };

    let mut blockers = Vec::new();
    if host.kind == RuntimeTerminalHostKind::Embedded {
        blockers.push("embedded host is the production default".to_string());
    }
    if host.command_routing != RuntimeTerminalHostCommandRouting::TerminalHost {
        blockers.push("daemon commands still route to mux".to_string());
    }
    if host.pane_ownership != RuntimeTerminalHostPaneOwnership::Host {
        blockers.push("agent panes are still mux-owned".to_string());
    }
    if host.agent_factory != RuntimeTerminalHostAgentFactoryRouting::TerminalHost {
        blockers.push("worker/reviewer/supervisor factory still mux-owned".to_string());
    }
    match host.capabilities.as_ref() {
        Some(capabilities) => {
            if !capabilities.absolute_resize {
                blockers.push("terminal host does not advertise absolute resize".to_string());
            }
        }
        None => blockers.push("terminal-host capabilities are missing".to_string()),
    }

    RuntimeTerminalHostPromotionReadiness {
        ready: blockers.is_empty(),
        blockers,
    }
}

pub fn terminal_host_diagnostics(
    host: Option<&RuntimeTerminalHostStatus>,
    registry: &PaneRegistrySnapshot,
    running: bool,
) -> Vec<RuntimeTerminalHostDiagnostic> {
    let Some(host) = host else {
        return vec![RuntimeTerminalHostDiagnostic {
            severity: RuntimeTerminalHostDiagnosticSeverity::Warning,
            code: "terminal_host_status_missing".to_string(),
            message: "terminal host status is missing".to_string(),
            action: Some(
                "restart brehon run so the launcher can publish terminal-host status".to_string(),
            ),
        }];
    };
    if host.kind == RuntimeTerminalHostKind::Embedded {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    if let Some(binary_path) = host.binary_path.as_deref()
        && terminal_host_binary_path_is_missing(binary_path)
    {
        diagnostics.push(RuntimeTerminalHostDiagnostic {
            severity: RuntimeTerminalHostDiagnosticSeverity::Error,
            code: "terminal_host_binary_missing".to_string(),
            message: format!("terminal host binary '{binary_path}' does not exist"),
            action: Some("set the terminal-host binary path to a valid executable".to_string()),
        });
    }

    if host
        .capabilities
        .as_ref()
        .is_some_and(|capabilities| !capabilities.absolute_resize)
    {
        diagnostics.push(RuntimeTerminalHostDiagnostic {
            severity: RuntimeTerminalHostDiagnosticSeverity::Warning,
            code: "terminal_host_absolute_resize_unsupported".to_string(),
            message: format!("{:?} does not advertise absolute resize", host.kind),
            action: Some(
                "keep promotion blocked or use a host that supports absolute pane resize"
                    .to_string(),
            ),
        });
    }

    let host_loss_panes = registry
        .panes
        .iter()
        .filter(|pane| {
            pane.state == RuntimePaneState::Dead
                && pane
                    .source
                    .as_ref()
                    .is_some_and(|source| source_matches_terminal_host(host.kind, source))
                && pane
                    .exit_reason
                    .as_deref()
                    .is_some_and(terminal_host_exit_reason_is_host_loss)
        })
        .collect::<Vec<_>>();
    if running && !host_loss_panes.is_empty() {
        let first = host_loss_panes[0];
        let reason = first.exit_reason.as_deref().unwrap_or("host disappeared");
        diagnostics.push(RuntimeTerminalHostDiagnostic {
            severity: RuntimeTerminalHostDiagnosticSeverity::Error,
            code: "terminal_host_session_lost".to_string(),
            message: format!(
                "{} {:?} pane(s) are dead after terminal-host loss; first={}/{} reason={reason}",
                host_loss_panes.len(),
                host.kind,
                first.session_id,
                first.pane_id
            ),
            action: Some(
                "reattach to inspect the external host if it still exists, otherwise reset or recycle affected panes after restarting the host"
                    .to_string(),
            ),
        });
    }

    diagnostics
}

fn terminal_host_binary_path_is_missing(binary_path: &str) -> bool {
    let path = Path::new(binary_path);
    (path.is_absolute() || binary_path.contains(std::path::MAIN_SEPARATOR)) && !path.exists()
}

fn terminal_host_exit_reason_is_host_loss(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("disappeared")
        || reason.contains("no server running")
        || reason.contains("session lost")
        || reason.contains("host lost")
}

fn source_matches_terminal_host(kind: RuntimeTerminalHostKind, source: &RuntimeSource) -> bool {
    matches!(
        (kind, source),
        (
            RuntimeTerminalHostKind::Embedded,
            RuntimeSource::EmbeddedTui
        ) | (RuntimeTerminalHostKind::Headless, RuntimeSource::Headless)
            | (RuntimeTerminalHostKind::Web, RuntimeSource::Web)
            | (RuntimeTerminalHostKind::NativeGui, RuntimeSource::NativeGui)
    )
}

impl RuntimeSidecarStatusHandle {
    fn new() -> Self {
        Self {
            inner: Arc::new(RuntimeSidecarState {
                detection_running: AtomicBool::new(true),
                workflow_running: AtomicBool::new(true),
            }),
        }
    }

    pub fn snapshot(&self) -> RuntimeSidecarStatus {
        RuntimeSidecarStatus {
            detection_running: self.inner.detection_running.load(Ordering::SeqCst),
            workflow_running: self.inner.workflow_running.load(Ordering::SeqCst),
        }
    }
}

impl RuntimeSidecar {
    /// Start the default sidecar pipeline: daemon fanout -> detection ->
    /// workflow audit events.
    pub fn start_default(daemon: RuntimeDaemon) -> Self {
        Self::start(
            daemon,
            Arc::new(PatternDetectionEngine::default()),
            WorkflowEngine::default(),
        )
    }

    /// Start the default sidecar pipeline with an explicit workflow command
    /// allowlist. Empty ids keep workflows in dry-run/audit mode.
    pub fn start_with_enabled_workflows(
        daemon: RuntimeDaemon,
        workflow_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self::start(
            daemon,
            Arc::new(PatternDetectionEngine::default()),
            WorkflowEngine::default().with_enabled_workflows(workflow_ids),
        )
    }

    /// Start sidecar loops with explicit detector and workflow engine.
    pub fn start(
        daemon: RuntimeDaemon,
        detector: Arc<dyn DetectionEngine>,
        workflow_engine: WorkflowEngine,
    ) -> Self {
        let status = RuntimeSidecarStatusHandle::new();
        let mut detection_stream = daemon.subscribe();
        let detection_sink = daemon.clone();
        let detection_state = status.inner.clone();
        let detection_task = tokio::spawn(async move {
            let stats =
                run_detection_loop(&mut detection_stream, detector.as_ref(), &detection_sink).await;
            detection_state
                .detection_running
                .store(false, Ordering::SeqCst);
            stats
        });

        let mut workflow_stream = daemon.subscribe();
        let workflow_daemon = daemon.clone();
        let workflow_state = status.inner.clone();
        let workflow_task = tokio::spawn(async move {
            let stats =
                run_daemon_workflow_loop(&mut workflow_stream, &workflow_engine, &workflow_daemon)
                    .await;
            workflow_state
                .workflow_running
                .store(false, Ordering::SeqCst);
            stats
        });

        Self {
            daemon,
            detection_task,
            workflow_task,
            status,
        }
    }

    pub fn daemon(&self) -> &RuntimeDaemon {
        &self.daemon
    }

    pub fn status_handle(&self) -> RuntimeSidecarStatusHandle {
        self.status.clone()
    }

    pub fn status(&self) -> RuntimeSidecarStatus {
        self.status.snapshot()
    }

    /// Stop the daemon and abort sidecar loops.
    pub async fn shutdown(self) {
        self.daemon.shutdown().await;
        self.detection_task.abort();
        self.workflow_task.abort();
        let _ = self.detection_task.await;
        let _ = self.workflow_task.await;
        self.status
            .inner
            .detection_running
            .store(false, Ordering::SeqCst);
        self.status
            .inner
            .workflow_running
            .store(false, Ordering::SeqCst);
    }
}

/// Periodic writer for the daemon's current health snapshot.
pub struct RuntimeDaemonHeartbeat {
    task: JoinHandle<()>,
}

/// File-backed command inbox for local non-TUI operators.
pub struct RuntimeDaemonCommandInbox {
    task: JoinHandle<()>,
}

impl RuntimeDaemonHeartbeat {
    pub fn start(
        path: PathBuf,
        daemon: RuntimeDaemon,
        sidecar: Option<RuntimeSidecarStatusHandle>,
        interval: Duration,
    ) -> Self {
        let interval = interval.max(Duration::from_secs(1));
        let task = tokio::spawn(async move {
            loop {
                if let Err(err) = Self::write_current_status(&path, &daemon, sidecar.as_ref()).await
                {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "Failed to write runtime daemon heartbeat"
                    );
                }
                tokio::time::sleep(interval).await;
            }
        });
        Self { task }
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }

    pub async fn write_current_status(
        path: impl AsRef<Path>,
        daemon: &RuntimeDaemon,
        sidecar: Option<&RuntimeSidecarStatusHandle>,
    ) -> Result<(), PortError> {
        let path = path.as_ref();
        let status = daemon
            .status(sidecar.map(RuntimeSidecarStatusHandle::snapshot))
            .await;
        let encoded = serde_json::to_vec_pretty(&status).map_err(|err| {
            PortError::Runtime(format!("failed to encode runtime daemon status: {err}"))
        })?;
        write_atomic(path, encoded).await
    }
}

impl RuntimeDaemonCommandInbox {
    pub fn start(root: PathBuf, daemon: RuntimeDaemon, interval: Duration) -> Self {
        let interval = interval.max(Duration::from_millis(250));
        let task = tokio::spawn(async move {
            loop {
                if let Err(err) = process_command_inbox_once(&root, &daemon).await {
                    tracing::warn!(
                        path = %root.display(),
                        error = %err,
                        "Failed to process runtime daemon command inbox"
                    );
                }
                tokio::time::sleep(interval).await;
            }
        });
        Self { task }
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

struct RuntimeDaemonInner {
    bus: RuntimeEventBus,
    registry: RwLock<HashMap<PaneRegistryKey, PaneRegistryEntry>>,
    approvals: RwLock<HashMap<String, PendingApprovalEntry>>,
    policy_gate: Option<Arc<dyn PolicyGate>>,
    command_port: Option<Arc<dyn RuntimeCommandPort>>,
    audit_log_path: Option<PathBuf>,
    audit_write_lock: Mutex<()>,
    approval_store_path: Option<PathBuf>,
    approval_store_session_id: Option<String>,
    approval_store_write_lock: Mutex<()>,
    terminal_host: Option<RuntimeTerminalHostStatus>,
    counters: Mutex<RuntimeDaemonCounters>,
    started_at_ms: u64,
    stopped: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PaneRegistryKey {
    session_id: String,
    pane_id: String,
}

/// Current daemon view of one pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRegistryEntry {
    pub session_id: String,
    pub pane_id: String,
    pub generation: u64,
    pub state: RuntimePaneState,
    pub kind: RuntimePaneKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RuntimeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub last_event_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_reason: Option<String>,
}

/// Point-in-time daemon pane registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRegistrySnapshot {
    pub generated_at_ms: u64,
    pub panes: Vec<PaneRegistryEntry>,
}

/// Command waiting for an explicit approval decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApprovalEntry {
    pub approval_id: String,
    pub requested_at_ms: u64,
    pub reason: String,
    pub command: RuntimeCommand,
    #[serde(default)]
    pub context: RuntimePolicyContext,
}

/// Point-in-time pending approval registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRegistrySnapshot {
    pub generated_at_ms: u64,
    pub approvals: Vec<PendingApprovalEntry>,
}

/// Current on-disk pending approval store format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalStoreSnapshot {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub written_at_ms: u64,
    #[serde(default)]
    pub approvals: Vec<PendingApprovalEntry>,
}

/// Result of loading the pending approval store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApprovalStoreLoadReport {
    pub loaded: usize,
    pub ignored_stale: usize,
}

/// File-backed command request consumed by the runtime daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCommandInboxRequest {
    pub request_id: String,
    pub created_at_ms: u64,
    pub command: RuntimeCommand,
    #[serde(default)]
    pub context: RuntimePolicyContext,
}

/// File-backed command result written by the runtime daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCommandInboxResult {
    pub request_id: String,
    pub completed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RuntimeCommandResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Current daemon health and observability snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDaemonStatus {
    pub generated_at_ms: u64,
    pub started_at_ms: u64,
    pub uptime_ms: u64,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_store_path: Option<String>,
    pub metrics: RuntimeDaemonMetrics,
    pub registry_count: usize,
    pub registry: PaneRegistrySnapshot,
    pub approvals: ApprovalRegistrySnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<RuntimeSidecarStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_host: Option<RuntimeTerminalHostStatus>,
}

/// Observable daemon counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDaemonMetrics {
    pub event_capacity: usize,
    pub subscriber_count: usize,
    pub published_events: u64,
    pub routed_commands: u64,
    pub rejected_commands: u64,
    pub deferred_commands: u64,
    pub approval_required_commands: u64,
    pub pending_approvals: usize,
    pub audit_write_errors: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct RuntimeDaemonCounters {
    published_events: u64,
    routed_commands: u64,
    rejected_commands: u64,
    deferred_commands: u64,
    approval_required_commands: u64,
    audit_write_errors: u64,
}

impl Default for RuntimeDaemon {
    fn default() -> Self {
        Self::new(RuntimeDaemonConfig::default())
    }
}

impl RuntimeDaemon {
    /// Start a sidecar daemon instance.
    pub fn new(config: RuntimeDaemonConfig) -> Self {
        let capacity = config
            .event_capacity
            .unwrap_or(DEFAULT_RUNTIME_EVENT_CAPACITY)
            .max(1);
        let terminal_host = config.terminal_host.map(|mut host| {
            host.promotion_readiness = terminal_host_promotion_readiness(Some(&host));
            host
        });
        Self {
            inner: Arc::new(RuntimeDaemonInner {
                bus: RuntimeEventBus::new(capacity),
                registry: RwLock::new(HashMap::new()),
                approvals: RwLock::new(HashMap::new()),
                policy_gate: config.policy_gate,
                command_port: config.command_port,
                audit_log_path: config.audit_log_path,
                audit_write_lock: Mutex::new(()),
                approval_store_path: config.approval_store_path,
                approval_store_session_id: config.approval_store_session_id,
                approval_store_write_lock: Mutex::new(()),
                terminal_host,
                counters: Mutex::new(RuntimeDaemonCounters::default()),
                started_at_ms: unix_timestamp_ms(),
                stopped: AtomicBool::new(false),
            }),
        }
    }

    /// Start a sidecar daemon and restore persisted pending approvals.
    pub async fn new_with_persisted_approvals(
        config: RuntimeDaemonConfig,
    ) -> Result<(Self, ApprovalStoreLoadReport), PortError> {
        let daemon = Self::new(config);
        let report = daemon.load_persisted_approvals().await?;
        Ok((daemon, report))
    }

    /// Subscribe to future daemon runtime events.
    pub fn subscribe(&self) -> RuntimeEventReceiver {
        self.inner.bus.subscribe()
    }

    /// Restore pending approvals from the configured approval store.
    pub async fn load_persisted_approvals(&self) -> Result<ApprovalStoreLoadReport, PortError> {
        let Some(path) = self.inner.approval_store_path.as_ref() else {
            return Ok(ApprovalStoreLoadReport::default());
        };

        let contents = match tokio::fs::read_to_string(path).await {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.persist_approval_store().await?;
                return Ok(ApprovalStoreLoadReport::default());
            }
            Err(err) => {
                return Err(PortError::Runtime(format!(
                    "failed to read pending approval store {}: {err}",
                    path.display()
                )));
            }
        };

        let store = serde_json::from_str::<ApprovalStoreSnapshot>(&contents).map_err(|err| {
            PortError::Runtime(format!(
                "failed to decode pending approval store {}: {err}",
                path.display()
            ))
        })?;
        if store.schema_version != APPROVAL_STORE_SCHEMA_VERSION {
            return Err(PortError::Runtime(format!(
                "unsupported pending approval store schema {} in {}",
                store.schema_version,
                path.display()
            )));
        }

        let configured_session = self.inner.approval_store_session_id.as_deref();
        if let Some(session_id) = configured_session
            && store.session_id.as_deref() != Some(session_id)
        {
            self.inner.approvals.write().await.clear();
            self.persist_approval_store().await?;
            return Ok(ApprovalStoreLoadReport {
                loaded: 0,
                ignored_stale: store.approvals.len(),
            });
        }

        let mut ignored_stale = 0usize;
        let mut approvals = HashMap::new();
        for entry in store.approvals {
            if approval_matches_session(&entry, configured_session) {
                approvals.insert(entry.approval_id.clone(), entry);
            } else {
                ignored_stale = ignored_stale.saturating_add(1);
            }
        }
        let loaded = approvals.len();
        *self.inner.approvals.write().await = approvals;
        if ignored_stale > 0 {
            self.persist_approval_store().await?;
        }
        Ok(ApprovalStoreLoadReport {
            loaded,
            ignored_stale,
        })
    }

    /// Return a sorted pane registry snapshot.
    pub async fn pane_registry_snapshot(&self) -> PaneRegistrySnapshot {
        let mut panes: Vec<_> = self.inner.registry.read().await.values().cloned().collect();
        panes.sort_by(|a, b| {
            a.session_id
                .cmp(&b.session_id)
                .then_with(|| a.pane_id.cmp(&b.pane_id))
        });
        PaneRegistrySnapshot {
            generated_at_ms: unix_timestamp_ms(),
            panes,
        }
    }

    /// Return bounded-fanout and command-routing metrics.
    pub async fn metrics(&self) -> RuntimeDaemonMetrics {
        let counters = *self.inner.counters.lock().await;
        let pending_approvals = self.inner.approvals.read().await.len();
        RuntimeDaemonMetrics {
            event_capacity: self.inner.bus.capacity(),
            subscriber_count: self.inner.bus.receiver_count(),
            published_events: counters.published_events,
            routed_commands: counters.routed_commands,
            rejected_commands: counters.rejected_commands,
            deferred_commands: counters.deferred_commands,
            approval_required_commands: counters.approval_required_commands,
            pending_approvals,
            audit_write_errors: counters.audit_write_errors,
        }
    }

    /// Return a sorted pending approval snapshot.
    pub async fn approval_registry_snapshot(&self) -> ApprovalRegistrySnapshot {
        let mut approvals: Vec<_> = self
            .inner
            .approvals
            .read()
            .await
            .values()
            .cloned()
            .collect();
        approvals.sort_by(|a, b| a.approval_id.cmp(&b.approval_id));
        ApprovalRegistrySnapshot {
            generated_at_ms: unix_timestamp_ms(),
            approvals,
        }
    }

    fn command_inbox_session_error(&self, request: &RuntimeCommandInboxRequest) -> Option<String> {
        let expected = self.inner.approval_store_session_id.as_deref()?;
        let actual = request.command.target.session_id.as_str();
        if actual == expected {
            return None;
        }
        Some(format!(
            "stale command request targets session '{actual}', but active daemon session is '{expected}'"
        ))
    }

    /// Return daemon health, counters, registry, and optional sidecar status.
    pub async fn status(&self, sidecar: Option<RuntimeSidecarStatus>) -> RuntimeDaemonStatus {
        let generated_at_ms = unix_timestamp_ms();
        let registry = self.pane_registry_snapshot().await;
        let approvals = self.approval_registry_snapshot().await;
        let running = !self.inner.stopped.load(Ordering::SeqCst);
        let terminal_host = self.inner.terminal_host.clone().map(|mut host| {
            host.promotion_readiness = terminal_host_promotion_readiness(Some(&host));
            host.diagnostics = terminal_host_diagnostics(Some(&host), &registry, running);
            host
        });
        RuntimeDaemonStatus {
            generated_at_ms,
            started_at_ms: self.inner.started_at_ms,
            uptime_ms: generated_at_ms.saturating_sub(self.inner.started_at_ms),
            running,
            audit_log_path: self
                .inner
                .audit_log_path
                .as_ref()
                .map(|path| path.display().to_string()),
            approval_store_path: self
                .inner
                .approval_store_path
                .as_ref()
                .map(|path| path.display().to_string()),
            metrics: self.metrics().await,
            registry_count: registry.panes.len(),
            registry,
            approvals,
            sidecar,
            terminal_host,
        }
    }

    /// Stop accepting events and commands.
    pub async fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
    }

    /// Route one command through policy and then to the configured command port.
    pub async fn route_command(
        &self,
        command: RuntimeCommand,
        context: RuntimePolicyContext,
    ) -> Result<RuntimeCommandResult, PortError> {
        self.ensure_running()?;

        {
            let mut counters = self.inner.counters.lock().await;
            counters.routed_commands = counters.routed_commands.saturating_add(1);
        }

        if let brehon_types::RuntimeCommandKind::ResolveApproval {
            approval_id,
            approved,
        } = &command.kind
        {
            let decision = RuntimePolicyDecision::Allow;
            self.publish_policy_decision(&command, &decision).await?;
            return self
                .resolve_approval(command.command_id.clone(), approval_id, *approved)
                .await;
        }

        let context = self.hydrate_policy_context(&command, context).await;
        let decision = match self.registry_generation_decision(&command).await {
            Some(decision) => decision,
            None => self.evaluate_policy(command.clone(), context.clone()).await,
        };
        self.publish_policy_decision(&command, &decision).await?;

        match decision {
            RuntimePolicyDecision::Allow => {
                if let Some(command_port) = self.inner.command_port.as_ref() {
                    command_port.execute(command).await
                } else {
                    Ok(RuntimeCommandResult {
                        command_id: command.command_id,
                        status: RuntimeCommandStatus::Accepted,
                        message: Some(
                            "command accepted by daemon sidecar; no command port configured"
                                .to_string(),
                        ),
                    })
                }
            }
            RuntimePolicyDecision::Deny { reason } => {
                self.count_rejected().await;
                Ok(RuntimeCommandResult {
                    command_id: command.command_id,
                    status: RuntimeCommandStatus::Rejected,
                    message: Some(reason),
                })
            }
            RuntimePolicyDecision::Defer {
                retry_after_ms,
                reason,
            } => {
                {
                    let mut counters = self.inner.counters.lock().await;
                    counters.deferred_commands = counters.deferred_commands.saturating_add(1);
                }
                Ok(RuntimeCommandResult {
                    command_id: command.command_id,
                    status: RuntimeCommandStatus::Deferred,
                    message: Some(format!("{reason}; retry after {retry_after_ms}ms")),
                })
            }
            RuntimePolicyDecision::RequireApproval { reason } => {
                {
                    let mut counters = self.inner.counters.lock().await;
                    counters.approval_required_commands =
                        counters.approval_required_commands.saturating_add(1);
                }
                let approval_id = self
                    .store_pending_approval(command.clone(), context, reason.clone())
                    .await?;
                Ok(RuntimeCommandResult {
                    command_id: command.command_id,
                    status: RuntimeCommandStatus::Deferred,
                    message: Some(format!("approval required ({approval_id}): {reason}")),
                })
            }
        }
    }

    async fn store_pending_approval(
        &self,
        command: RuntimeCommand,
        context: RuntimePolicyContext,
        reason: String,
    ) -> Result<String, PortError> {
        let approval_id = format!("approval-{}", uuid::Uuid::new_v4());
        let entry = PendingApprovalEntry {
            approval_id: approval_id.clone(),
            requested_at_ms: unix_timestamp_ms(),
            reason,
            command,
            context,
        };
        {
            self.inner
                .approvals
                .write()
                .await
                .insert(approval_id.clone(), entry);
        }
        if let Err(err) = self.persist_approval_store().await {
            self.inner.approvals.write().await.remove(&approval_id);
            return Err(err);
        }
        Ok(approval_id)
    }

    async fn resolve_approval(
        &self,
        resolver_command_id: String,
        approval_id: &str,
        approved: bool,
    ) -> Result<RuntimeCommandResult, PortError> {
        let Some(pending) = self.inner.approvals.write().await.remove(approval_id) else {
            self.count_rejected().await;
            return Ok(RuntimeCommandResult {
                command_id: resolver_command_id,
                status: RuntimeCommandStatus::Rejected,
                message: Some(format!("approval '{approval_id}' was not found")),
            });
        };
        if let Err(err) = self.persist_approval_store().await {
            self.inner
                .approvals
                .write()
                .await
                .insert(approval_id.to_string(), pending);
            return Err(err);
        }

        if !approved {
            self.count_rejected().await;
            let decision = RuntimePolicyDecision::Deny {
                reason: format!("operator denied approval '{approval_id}'"),
            };
            self.publish_policy_decision(&pending.command, &decision)
                .await?;
            return Ok(RuntimeCommandResult {
                command_id: resolver_command_id,
                status: RuntimeCommandStatus::Rejected,
                message: Some(format!("approval '{approval_id}' denied")),
            });
        }

        let mut approved_context = self
            .hydrate_policy_context(&pending.command, pending.context.clone())
            .await;
        approved_context.approval_required = false;
        let decision = match self.registry_generation_decision(&pending.command).await {
            Some(decision) => decision,
            None => {
                self.evaluate_policy(pending.command.clone(), approved_context.clone())
                    .await
            }
        };
        self.publish_policy_decision(&pending.command, &decision)
            .await?;
        match decision {
            RuntimePolicyDecision::Allow => {}
            RuntimePolicyDecision::Deny { reason } => {
                self.count_rejected().await;
                return Ok(RuntimeCommandResult {
                    command_id: resolver_command_id,
                    status: RuntimeCommandStatus::Rejected,
                    message: Some(format!(
                        "approval '{approval_id}' was granted, but policy rejected the command: {reason}"
                    )),
                });
            }
            RuntimePolicyDecision::Defer {
                retry_after_ms,
                reason,
            } => {
                {
                    let mut counters = self.inner.counters.lock().await;
                    counters.deferred_commands = counters.deferred_commands.saturating_add(1);
                }
                return Ok(RuntimeCommandResult {
                    command_id: resolver_command_id,
                    status: RuntimeCommandStatus::Deferred,
                    message: Some(format!(
                        "approval '{approval_id}' was granted, but policy deferred the command: {reason}; retry after {retry_after_ms}ms"
                    )),
                });
            }
            RuntimePolicyDecision::RequireApproval { reason } => {
                {
                    let mut counters = self.inner.counters.lock().await;
                    counters.approval_required_commands =
                        counters.approval_required_commands.saturating_add(1);
                }
                let new_approval_id = self
                    .store_pending_approval(
                        pending.command.clone(),
                        approved_context,
                        reason.clone(),
                    )
                    .await?;
                return Ok(RuntimeCommandResult {
                    command_id: resolver_command_id,
                    status: RuntimeCommandStatus::Deferred,
                    message: Some(format!(
                        "approval '{approval_id}' was granted, but policy requested approval again ({new_approval_id}): {reason}"
                    )),
                });
            }
        }

        let result = if let Some(command_port) = self.inner.command_port.as_ref() {
            command_port.execute(pending.command).await?
        } else {
            RuntimeCommandResult {
                command_id: pending.command.command_id,
                status: RuntimeCommandStatus::Accepted,
                message: Some(
                    "approved command accepted by daemon sidecar; no command port configured"
                        .to_string(),
                ),
            }
        };

        Ok(RuntimeCommandResult {
            command_id: resolver_command_id,
            status: result.status,
            message: Some(format!(
                "approval '{approval_id}' applied to {}: {}",
                result.command_id,
                result
                    .message
                    .unwrap_or_else(|| "no command message".to_string())
            )),
        })
    }

    async fn evaluate_policy(
        &self,
        command: RuntimeCommand,
        context: RuntimePolicyContext,
    ) -> RuntimePolicyDecision {
        let Some(policy_gate) = self.inner.policy_gate.as_ref() else {
            return RuntimePolicyDecision::Allow;
        };
        match policy_gate
            .evaluate(RuntimePolicyRequest { command, context })
            .await
        {
            Ok(decision) => decision,
            Err(err) => RuntimePolicyDecision::Deny {
                reason: format!("policy evaluation failed: {err}"),
            },
        }
    }

    async fn hydrate_policy_context(
        &self,
        command: &RuntimeCommand,
        mut context: RuntimePolicyContext,
    ) -> RuntimePolicyContext {
        if context.pane_state.is_none()
            && let Some(entry) = self.registry_entry_for_command(command).await
        {
            context.pane_state = Some(entry.state);
        }
        context
    }

    async fn registry_generation_decision(
        &self,
        command: &RuntimeCommand,
    ) -> Option<RuntimePolicyDecision> {
        let requested = command.target.generation?;
        let entry = self.registry_entry_for_command(command).await?;
        if requested == entry.generation {
            return None;
        }
        Some(RuntimePolicyDecision::Deny {
            reason: format!(
                "pane generation mismatch: requested {}, current {}",
                requested, entry.generation
            ),
        })
    }

    async fn registry_entry_for_command(
        &self,
        command: &RuntimeCommand,
    ) -> Option<PaneRegistryEntry> {
        let pane_id = command.target.pane_id.as_ref()?;
        self.inner
            .registry
            .read()
            .await
            .get(&PaneRegistryKey {
                session_id: command.target.session_id.clone(),
                pane_id: pane_id.clone(),
            })
            .cloned()
    }

    async fn publish_policy_decision(
        &self,
        command: &RuntimeCommand,
        decision: &RuntimePolicyDecision,
    ) -> Result<(), PortError> {
        let event = policy_decision_event(command, decision);
        self.publish_event(event).await
    }

    async fn publish_event(&self, event: RuntimeEvent) -> Result<(), PortError> {
        self.ensure_running()?;
        self.apply_registry_event(&event).await;
        self.append_audit_event(&event).await;
        self.inner.bus.publish(event).await?;
        let mut counters = self.inner.counters.lock().await;
        counters.published_events = counters.published_events.saturating_add(1);
        Ok(())
    }

    async fn append_audit_event(&self, event: &RuntimeEvent) {
        let Some(path) = self.inner.audit_log_path.as_ref() else {
            return;
        };
        let _guard = self.inner.audit_write_lock.lock().await;
        if let Some(parent) = path.parent()
            && let Err(err) = tokio::fs::create_dir_all(parent).await
        {
            self.count_audit_error().await;
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to create runtime audit log directory"
            );
            return;
        }

        let encoded = match serde_json::to_vec(event) {
            Ok(encoded) => encoded,
            Err(err) => {
                self.count_audit_error().await;
                tracing::warn!(error = %err, "Failed to encode runtime audit event");
                return;
            }
        };

        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
        {
            Ok(file) => file,
            Err(err) => {
                self.count_audit_error().await;
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "Failed to open runtime audit log"
                );
                return;
            }
        };

        if let Err(err) = file.write_all(&encoded).await {
            self.count_audit_error().await;
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to write runtime audit event"
            );
            return;
        }
        if let Err(err) = file.write_all(b"\n").await {
            self.count_audit_error().await;
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to terminate runtime audit event"
            );
        }
    }

    async fn apply_registry_event(&self, event: &RuntimeEvent) {
        let mut registry = self.inner.registry.write().await;
        let key = PaneRegistryKey {
            session_id: event.meta.session_id.clone(),
            pane_id: event.meta.pane_id.clone(),
        };
        if registry
            .get(&key)
            .is_some_and(|entry| event.meta.generation < entry.generation)
        {
            return;
        }

        let entry = registry.entry(key).or_insert_with(|| PaneRegistryEntry {
            session_id: event.meta.session_id.clone(),
            pane_id: event.meta.pane_id.clone(),
            generation: event.meta.generation,
            state: RuntimePaneState::Unknown,
            kind: RuntimePaneKind::Unknown,
            source: Some(event.meta.source.clone()),
            title: None,
            last_event_ms: event.meta.timestamp_ms,
            last_output_ms: None,
            exit_code: None,
            exit_reason: None,
        });

        if registry_event_is_mux_shadow_of_terminal_host(entry, event) {
            return;
        }

        if event.meta.generation > entry.generation {
            entry.state = RuntimePaneState::Unknown;
            entry.last_output_ms = None;
            entry.exit_code = None;
            entry.exit_reason = None;
        }
        entry.generation = event.meta.generation;
        if registry_event_updates_pane_source(&event.kind) {
            entry.source = Some(event.meta.source.clone());
        }
        entry.last_event_ms = event.meta.timestamp_ms;

        match &event.kind {
            RuntimeEventKind::PaneSpawned(PaneSpawnedEvent { kind, title }) => {
                entry.kind = kind.clone();
                entry.title = title.clone();
                entry.state = RuntimePaneState::Ready;
                entry.exit_code = None;
                entry.exit_reason = None;
            }
            RuntimeEventKind::PaneStateChanged(changed) => {
                entry.state = changed.current.clone();
            }
            RuntimeEventKind::PaneExited(PaneExitedEvent { exit_code, reason }) => {
                entry.state = RuntimePaneState::Dead;
                entry.exit_code = *exit_code;
                entry.exit_reason = reason.clone();
            }
            RuntimeEventKind::PaneOutput(_) => {
                entry.last_output_ms = Some(event.meta.timestamp_ms);
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

    fn ensure_running(&self) -> Result<(), PortError> {
        if self.inner.stopped.load(Ordering::SeqCst) {
            Err(PortError::Runtime("runtime daemon is stopped".to_string()))
        } else {
            Ok(())
        }
    }

    async fn count_rejected(&self) {
        let mut counters = self.inner.counters.lock().await;
        counters.rejected_commands = counters.rejected_commands.saturating_add(1);
    }

    async fn count_audit_error(&self) {
        let mut counters = self.inner.counters.lock().await;
        counters.audit_write_errors = counters.audit_write_errors.saturating_add(1);
    }

    async fn persist_approval_store(&self) -> Result<(), PortError> {
        let Some(path) = self.inner.approval_store_path.as_ref() else {
            return Ok(());
        };
        let _guard = self.inner.approval_store_write_lock.lock().await;
        let approvals = self.approval_registry_snapshot().await.approvals;
        let store = ApprovalStoreSnapshot {
            schema_version: APPROVAL_STORE_SCHEMA_VERSION,
            session_id: self.inner.approval_store_session_id.clone(),
            written_at_ms: unix_timestamp_ms(),
            approvals,
        };
        let encoded = serde_json::to_vec_pretty(&store).map_err(|err| {
            PortError::Runtime(format!("failed to encode pending approval store: {err}"))
        })?;
        write_atomic(path, encoded).await
    }
}

fn registry_event_updates_pane_source(kind: &RuntimeEventKind) -> bool {
    matches!(
        kind,
        RuntimeEventKind::PaneSpawned(_)
            | RuntimeEventKind::PaneStateChanged(_)
            | RuntimeEventKind::PaneExited(_)
            | RuntimeEventKind::PaneOutput(_)
            | RuntimeEventKind::AgentTurnStarted(_)
            | RuntimeEventKind::AgentTurnEnded(_)
            | RuntimeEventKind::PromptDelivered(_)
            | RuntimeEventKind::PromptRejected(_)
            | RuntimeEventKind::ActivityObserved(_)
    )
}

fn runtime_source_is_terminal_host(source: &RuntimeSource) -> bool {
    matches!(
        source,
        RuntimeSource::EmbeddedTui
            | RuntimeSource::Web
            | RuntimeSource::NativeGui
            | RuntimeSource::Headless
    )
}

fn registry_event_is_mux_shadow_of_terminal_host(
    entry: &PaneRegistryEntry,
    event: &RuntimeEvent,
) -> bool {
    event.meta.source == RuntimeSource::Mux
        && event.meta.generation <= entry.generation
        && entry
            .source
            .as_ref()
            .is_some_and(runtime_source_is_terminal_host)
        && matches!(
            &event.kind,
            RuntimeEventKind::PaneSpawned(_)
                | RuntimeEventKind::PaneStateChanged(_)
                | RuntimeEventKind::PaneExited(_)
                | RuntimeEventKind::PaneOutput(_)
        )
}

#[async_trait]
impl RuntimeEventSink for RuntimeDaemon {
    async fn publish(&self, event: RuntimeEvent) -> Result<(), PortError> {
        self.publish_event(event).await
    }
}

#[async_trait]
impl RuntimeCommandRouter for RuntimeDaemon {
    async fn route_command(
        &self,
        command: RuntimeCommand,
        context: RuntimePolicyContext,
    ) -> Result<RuntimeCommandResult, PortError> {
        RuntimeDaemon::route_command(self, command, context).await
    }
}

async fn run_daemon_workflow_loop(
    stream: &mut dyn RuntimeEventStream,
    engine: &WorkflowEngine,
    daemon: &RuntimeDaemon,
) -> WorkflowLoopStats {
    let mut stats = WorkflowLoopStats::default();
    loop {
        let event = match stream.next_event().await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(err) => {
                stats.stream_errors = stats.stream_errors.saturating_add(1);
                tracing::warn!(error = %err, "Runtime workflow stream error");
                tokio::task::yield_now().await;
                continue;
            }
        };

        stats.observed_events = stats.observed_events.saturating_add(1);
        for emission in engine.observe_emissions(event) {
            stats.emitted_actions = stats.emitted_actions.saturating_add(1);
            let brehon_workflow::WorkflowEmission {
                audit_event,
                command,
                policy_context,
            } = emission;

            if let Err(err) = daemon.publish(audit_event).await {
                stats.publish_errors = stats.publish_errors.saturating_add(1);
                if command.is_some() {
                    stats.command_errors = stats.command_errors.saturating_add(1);
                }
                tracing::warn!(error = %err, "Failed to publish workflow action");
                continue;
            }

            if let Some(command) = command {
                stats.routed_commands = stats.routed_commands.saturating_add(1);
                if let Err(err) = daemon.route_command(command, policy_context).await {
                    stats.command_errors = stats.command_errors.saturating_add(1);
                    tracing::warn!(error = %err, "Failed to route workflow command");
                }
            }
        }
    }
    stats
}

/// Process pending file-backed daemon commands once.
pub async fn process_command_inbox_once(
    root: impl AsRef<Path>,
    daemon: &RuntimeDaemon,
) -> Result<usize, PortError> {
    let root = root.as_ref();
    let pending_dir = root.join("pending");
    let results_dir = root.join("results");
    tokio::fs::create_dir_all(&pending_dir).await?;
    tokio::fs::create_dir_all(&results_dir).await?;

    let mut entries = tokio::fs::read_dir(&pending_dir).await?;
    let mut pending_files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        pending_files.push(path);
    }
    pending_files.sort();

    let mut processed = 0usize;
    for path in pending_files {
        let fallback_request_id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        let completion = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => match serde_json::from_str::<RuntimeCommandInboxRequest>(&contents) {
                Ok(request) => {
                    let result_path = command_inbox_result_path(&results_dir, &request.request_id);
                    if command_inbox_result_exists(&result_path).await? {
                        remove_command_inbox_file(&path).await?;
                        processed = processed.saturating_add(1);
                        continue;
                    }
                    if let Some(error) = command_inbox_request_id_error(&request.request_id)
                        .or_else(|| daemon.command_inbox_session_error(&request))
                    {
                        RuntimeCommandInboxResult {
                            request_id: request.request_id,
                            completed_at_ms: unix_timestamp_ms(),
                            result: None,
                            error: Some(error),
                        }
                    } else {
                        let result = daemon.route_command(request.command, request.context).await;
                        match result {
                            Ok(result) => RuntimeCommandInboxResult {
                                request_id: request.request_id,
                                completed_at_ms: unix_timestamp_ms(),
                                result: Some(result),
                                error: None,
                            },
                            Err(err) => RuntimeCommandInboxResult {
                                request_id: request.request_id,
                                completed_at_ms: unix_timestamp_ms(),
                                result: None,
                                error: Some(err.to_string()),
                            },
                        }
                    }
                }
                Err(err) => RuntimeCommandInboxResult {
                    request_id: fallback_request_id,
                    completed_at_ms: unix_timestamp_ms(),
                    result: None,
                    error: Some(format!("failed to decode command request: {err}")),
                },
            },
            Err(err) => RuntimeCommandInboxResult {
                request_id: fallback_request_id,
                completed_at_ms: unix_timestamp_ms(),
                result: None,
                error: Some(format!("failed to read command request: {err}")),
            },
        };

        let result_path = command_inbox_result_path(&results_dir, &completion.request_id);
        if command_inbox_result_exists(&result_path).await? {
            remove_command_inbox_file(&path).await?;
            processed = processed.saturating_add(1);
            continue;
        }
        let encoded = serde_json::to_vec_pretty(&completion).map_err(|err| {
            PortError::Runtime(format!("failed to encode command inbox result: {err}"))
        })?;
        write_atomic(&result_path, encoded).await?;
        remove_command_inbox_file(&path).await?;
        processed = processed.saturating_add(1);
    }
    Ok(processed)
}

/// Read a daemon JSONL audit log into runtime events.
pub async fn read_audit_log(path: impl AsRef<Path>) -> Result<Vec<RuntimeEvent>, PortError> {
    let path = path.as_ref();
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(PortError::Runtime(format!(
                "failed to read runtime audit log {}: {err}",
                path.display()
            )));
        }
    };

    let mut events = Vec::new();
    for (idx, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<RuntimeEvent>(line).map_err(|err| {
            PortError::Runtime(format!(
                "failed to decode runtime audit log {} line {}: {err}",
                path.display(),
                idx + 1
            ))
        })?;
        events.push(event);
    }
    Ok(events)
}

/// Replay a daemon JSONL audit log into another runtime event sink.
pub async fn replay_audit_log(
    path: impl AsRef<Path>,
    sink: &dyn RuntimeEventSink,
) -> Result<usize, PortError> {
    let events = read_audit_log(path).await?;
    let count = events.len();
    sink.publish_batch(events).await?;
    Ok(count)
}

fn policy_decision_event(
    command: &RuntimeCommand,
    decision: &RuntimePolicyDecision,
) -> RuntimeEvent {
    let pane_id = command
        .target
        .pane_id
        .clone()
        .unwrap_or_else(|| "runtime".to_string());
    let meta = RuntimeEventMeta::new(
        command.target.session_id.clone(),
        pane_id,
        command.target.generation.unwrap_or_default(),
        RuntimeSource::Policy,
        unix_timestamp_ms(),
    )
    .with_correlation_id(command.command_id.clone());

    RuntimeEvent::new(
        meta,
        RuntimeEventKind::PolicyDecision(PolicyDecisionEvent {
            decision_id: format!("decision-{}", uuid::Uuid::new_v4()),
            operation: runtime_operation_for_command(command),
            decision: decision.clone(),
        }),
    )
}

fn runtime_operation_for_command(command: &RuntimeCommand) -> RuntimeOperation {
    match &command.kind {
        brehon_types::RuntimeCommandKind::SendPrompt { .. } => RuntimeOperation::SendPrompt,
        brehon_types::RuntimeCommandKind::BroadcastPrompt { .. } => {
            RuntimeOperation::BroadcastPrompt
        }
        brehon_types::RuntimeCommandKind::SendTerminalInput { .. } => {
            RuntimeOperation::SendTerminalInput
        }
        brehon_types::RuntimeCommandKind::Interrupt { .. } => RuntimeOperation::Interrupt,
        brehon_types::RuntimeCommandKind::ResetPane { .. } => RuntimeOperation::ResetPane,
        brehon_types::RuntimeCommandKind::RecyclePane { .. } => RuntimeOperation::RecyclePane,
        brehon_types::RuntimeCommandKind::QuarantinePane { .. } => RuntimeOperation::QuarantinePane,
        brehon_types::RuntimeCommandKind::SpawnPane { .. } => RuntimeOperation::SpawnPane,
        brehon_types::RuntimeCommandKind::ResizePane { .. } => RuntimeOperation::ResizePane,
        brehon_types::RuntimeCommandKind::ClosePane { .. } => RuntimeOperation::ClosePane,
        brehon_types::RuntimeCommandKind::ResolveApproval { .. } => {
            RuntimeOperation::ResolveApproval
        }
    }
}

fn approval_matches_session(entry: &PendingApprovalEntry, session_id: Option<&str>) -> bool {
    session_id
        .map(|session_id| entry.command.target.session_id == session_id)
        .unwrap_or(true)
}

fn safe_file_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "request".to_string()
    } else {
        sanitized
    }
}

fn command_inbox_result_path(results_dir: &Path, request_id: &str) -> PathBuf {
    results_dir.join(format!("{}.json", safe_file_component(request_id)))
}

fn command_inbox_request_id_error(request_id: &str) -> Option<String> {
    if request_id.is_empty() {
        return Some("command request_id must not be empty".to_string());
    }
    if request_id.len() > MAX_COMMAND_INBOX_REQUEST_ID_LEN {
        return Some(format!(
            "command request_id is too long ({} bytes, max {MAX_COMMAND_INBOX_REQUEST_ID_LEN})",
            request_id.len()
        ));
    }
    if request_id != safe_file_component(request_id) {
        return Some(format!(
            "command request_id {request_id:?} contains unsupported file-name characters; use ASCII letters, digits, '-', or '_'"
        ));
    }
    None
}

async fn command_inbox_result_exists(path: &Path) -> Result<bool, PortError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(PortError::Runtime(format!(
            "command inbox result path is not a file: {}",
            path.display()
        ))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(PortError::Runtime(format!(
            "failed to inspect command inbox result at {}: {err}",
            path.display()
        ))),
    }
}

async fn remove_command_inbox_file(path: &Path) -> Result<(), PortError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(PortError::Runtime(format!(
            "failed to remove command inbox request at {}: {err}",
            path.display()
        ))),
    }
}

async fn write_atomic(path: &Path, encoded: Vec<u8>) -> Result<(), PortError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp_path = path.with_extension(
        path.extension()
            .map(|ext| format!("{}.tmp", ext.to_string_lossy()))
            .unwrap_or_else(|| "tmp".to_string()),
    );
    tokio::fs::write(&tmp_path, encoded).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_ports::{RuntimeCommandPort, RuntimeEventSink, RuntimeEventStream};
    use brehon_types::{
        DetectionEvent, DetectionSeverity, PaneOutputEvent, PaneStateChangedEvent,
        PromptDeliveryMode, RuntimeCommandKind, RuntimeCommandTarget, WorkflowActionStatus,
    };

    #[derive(Default)]
    struct RecordingCommandPort {
        commands: tokio::sync::Mutex<Vec<RuntimeCommand>>,
    }

    #[async_trait]
    impl RuntimeCommandPort for RecordingCommandPort {
        async fn execute(
            &self,
            command: RuntimeCommand,
        ) -> Result<RuntimeCommandResult, PortError> {
            let command_id = command.command_id.clone();
            self.commands.lock().await.push(command);
            Ok(RuntimeCommandResult {
                command_id,
                status: RuntimeCommandStatus::Applied,
                message: Some("recorded".to_string()),
            })
        }
    }

    fn meta(pane_id: &str, generation: u64) -> RuntimeEventMeta {
        RuntimeEventMeta::new(
            "session",
            pane_id,
            generation,
            RuntimeSource::Mux,
            generation,
        )
    }

    fn meta_with_source(
        pane_id: &str,
        generation: u64,
        source: RuntimeSource,
        timestamp_ms: u64,
    ) -> RuntimeEventMeta {
        RuntimeEventMeta::new("session", pane_id, generation, source, timestamp_ms)
    }

    fn command(kind: RuntimeCommandKind) -> RuntimeCommand {
        RuntimeCommand {
            command_id: "cmd".to_string(),
            target: RuntimeCommandTarget {
                session_id: "session".to_string(),
                pane_id: Some("pane".to_string()),
                generation: Some(1),
            },
            issued_at_ms: 1,
            kind,
        }
    }

    #[tokio::test]
    async fn daemon_fans_out_events_and_tracks_registry_snapshot() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            event_capacity: Some(8),
            ..RuntimeDaemonConfig::default()
        });
        let mut rx = daemon.subscribe();

        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ))
            .await
            .expect("publish spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"hello".to_vec(),
                    text: Some("hello".to_string()),
                }),
            ))
            .await
            .expect("publish output");

        let first = rx.next_event().await.expect("stream").expect("first event");
        assert!(matches!(first.kind, RuntimeEventKind::PaneSpawned(_)));

        let snapshot = daemon.pane_registry_snapshot().await;
        assert_eq!(snapshot.panes.len(), 1);
        assert_eq!(snapshot.panes[0].kind, RuntimePaneKind::Worker);
        assert_eq!(snapshot.panes[0].state, RuntimePaneState::Ready);
        assert_eq!(snapshot.panes[0].last_output_ms, Some(1));

        let metrics = daemon.metrics().await;
        assert_eq!(metrics.event_capacity, 8);
        assert_eq!(metrics.published_events, 2);
        assert_eq!(metrics.subscriber_count, 1);
    }

    #[tokio::test]
    async fn daemon_registry_rejects_stale_generation_events() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig::default());

        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("old".to_string()),
                }),
            ))
            .await
            .expect("publish gen1 spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"old output".to_vec(),
                    text: Some("old output".to_string()),
                }),
            ))
            .await
            .expect("publish gen1 output");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 2),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Shell,
                    title: Some("new".to_string()),
                }),
            ))
            .await
            .expect("publish gen2 spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 2),
                RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                    previous: None,
                    current: RuntimePaneState::Ready,
                    reason: Some("respawned".to_string()),
                }),
            ))
            .await
            .expect("publish gen2 ready");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: Some(99),
                    reason: Some("stale exit".to_string()),
                }),
            ))
            .await
            .expect("publish stale exit");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"stale output".to_vec(),
                    text: Some("stale output".to_string()),
                }),
            ))
            .await
            .expect("publish stale output");

        let snapshot = daemon.pane_registry_snapshot().await;
        assert_eq!(snapshot.panes.len(), 1);
        let pane = &snapshot.panes[0];
        assert_eq!(pane.generation, 2);
        assert_eq!(pane.state, RuntimePaneState::Ready);
        assert_eq!(pane.kind, RuntimePaneKind::Shell);
        assert_eq!(pane.source, Some(RuntimeSource::Mux));
        assert_eq!(pane.title.as_deref(), Some("new"));
        assert_eq!(pane.last_output_ms, None);
        assert_eq!(pane.exit_code, None);
        assert_eq!(pane.exit_reason, None);
    }

    #[tokio::test]
    async fn daemon_registry_preserves_identity_across_generation_state_event() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig::default());

        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Reviewer,
                    title: Some("reviewer-a".to_string()),
                }),
            ))
            .await
            .expect("publish gen1 spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 2),
                RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                    previous: Some(RuntimePaneState::Ready),
                    current: RuntimePaneState::Busy,
                    reason: Some("activity started".to_string()),
                }),
            ))
            .await
            .expect("publish gen2 state");

        let snapshot = daemon.pane_registry_snapshot().await;
        assert_eq!(snapshot.panes.len(), 1);
        let pane = &snapshot.panes[0];
        assert_eq!(pane.generation, 2);
        assert_eq!(pane.state, RuntimePaneState::Busy);
        assert_eq!(pane.kind, RuntimePaneKind::Reviewer);
        assert_eq!(pane.source, Some(RuntimeSource::Mux));
        assert_eq!(pane.title.as_deref(), Some("reviewer-a"));
    }

    #[tokio::test]
    async fn daemon_registry_keeps_owner_source_after_detection_event() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig::default());

        daemon
            .publish(RuntimeEvent::new(
                meta_with_source("pane", 1, RuntimeSource::Headless, 10),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker-a".to_string()),
                }),
            ))
            .await
            .expect("publish headless spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta_with_source("pane", 1, RuntimeSource::Headless, 11),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"approval requested".to_vec(),
                    text: Some("approval requested".to_string()),
                }),
            ))
            .await
            .expect("publish headless output");
        daemon
            .publish(RuntimeEvent::new(
                meta_with_source("pane", 1, RuntimeSource::Detector, 12),
                RuntimeEventKind::DetectionEvent(DetectionEvent {
                    detection_id: "session:pane:approval.prompt:1".to_string(),
                    rule_id: "approval.prompt".to_string(),
                    severity: DetectionSeverity::Warning,
                    message: "Agent is requesting approval".to_string(),
                    span: None,
                }),
            ))
            .await
            .expect("publish detector event");

        let snapshot = daemon.pane_registry_snapshot().await;
        assert_eq!(snapshot.panes.len(), 1);
        let pane = &snapshot.panes[0];
        assert_eq!(pane.source, Some(RuntimeSource::Headless));
        assert_eq!(pane.kind, RuntimePaneKind::Worker);
        assert_eq!(pane.state, RuntimePaneState::Ready);
        assert_eq!(pane.last_event_ms, 12);
        assert_eq!(pane.last_output_ms, Some(11));
    }

    #[tokio::test]
    async fn daemon_registry_ignores_mux_shadow_lifecycle_after_terminal_host() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig::default());

        daemon
            .publish(RuntimeEvent::new(
                meta_with_source("pane", 1, RuntimeSource::Headless, 10),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker-a".to_string()),
                }),
            ))
            .await
            .expect("publish headless spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta_with_source("pane", 1, RuntimeSource::Mux, 11),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("mux-placeholder".to_string()),
                }),
            ))
            .await
            .expect("publish mux placeholder spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta_with_source("pane", 1, RuntimeSource::Mux, 12),
                RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                    previous: Some(RuntimePaneState::Ready),
                    current: RuntimePaneState::Dead,
                    reason: Some("local placeholder dropped".to_string()),
                }),
            ))
            .await
            .expect("publish mux placeholder dead state");
        daemon
            .publish(RuntimeEvent::new(
                meta_with_source("pane", 1, RuntimeSource::Headless, 13),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"host output".to_vec(),
                    text: Some("host output".to_string()),
                }),
            ))
            .await
            .expect("publish headless output");

        let snapshot = daemon.pane_registry_snapshot().await;
        assert_eq!(snapshot.panes.len(), 1);
        let pane = &snapshot.panes[0];
        assert_eq!(pane.source, Some(RuntimeSource::Headless));
        assert_eq!(pane.kind, RuntimePaneKind::Worker);
        assert_eq!(pane.title.as_deref(), Some("worker-a"));
        assert_eq!(pane.state, RuntimePaneState::Ready);
        assert_eq!(pane.last_event_ms, 13);
        assert_eq!(pane.last_output_ms, Some(13));
        assert_eq!(pane.exit_reason, None);
    }

    #[tokio::test]
    async fn daemon_routes_commands_through_policy() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            ..RuntimeDaemonConfig::default()
        });
        let mut rx = daemon.subscribe();

        let result = daemon
            .route_command(
                command(RuntimeCommandKind::SendPrompt {
                    prompt_id: "p".to_string(),
                    text: "hello".to_string(),
                    from: None,
                    delivery: PromptDeliveryMode::Direct,
                }),
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Busy),
                    ..RuntimePolicyContext::default()
                },
            )
            .await
            .expect("route command");

        assert_eq!(result.status, RuntimeCommandStatus::Rejected);
        let audit = rx.next_event().await.expect("stream").expect("audit event");
        assert!(matches!(
            audit.kind,
            RuntimeEventKind::PolicyDecision(ref event)
                if matches!(event.decision, RuntimePolicyDecision::Deny { .. })
        ));

        let metrics = daemon.metrics().await;
        assert_eq!(metrics.routed_commands, 1);
        assert_eq!(metrics.rejected_commands, 1);
    }

    #[tokio::test]
    async fn daemon_policy_context_uses_registry_pane_state() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            ..RuntimeDaemonConfig::default()
        });
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ))
            .await
            .expect("publish spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: None,
                    reason: Some("dead".to_string()),
                }),
            ))
            .await
            .expect("publish exit");

        let result = daemon
            .route_command(
                command(RuntimeCommandKind::SendTerminalInput {
                    bytes: b"after".to_vec(),
                }),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route input");

        assert_eq!(result.status, RuntimeCommandStatus::Rejected);
        assert!(
            result
                .message
                .as_deref()
                .is_some_and(|message| message.contains("operation requires a live pane"))
        );
    }

    #[tokio::test]
    async fn daemon_rejects_generation_mismatch_before_command_port() {
        let command_port = Arc::new(RecordingCommandPort::default());
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port.clone()),
            ..RuntimeDaemonConfig::default()
        });
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 2),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ))
            .await
            .expect("publish spawn");
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 2),
                RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                    previous: None,
                    current: RuntimePaneState::Ready,
                    reason: Some("spawned".to_string()),
                }),
            ))
            .await
            .expect("publish ready");

        let result = daemon
            .route_command(
                command(RuntimeCommandKind::SendTerminalInput {
                    bytes: b"stale".to_vec(),
                }),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("route input");

        assert_eq!(result.status, RuntimeCommandStatus::Rejected);
        assert!(
            result
                .message
                .as_deref()
                .is_some_and(|message| message.contains("pane generation"))
        );
        assert!(command_port.commands.lock().await.is_empty());
    }

    #[tokio::test]
    async fn daemon_stores_and_resolves_pending_approval() {
        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });

        let result = daemon
            .route_command(
                command(RuntimeCommandKind::Interrupt {
                    reason: "operator".to_string(),
                }),
                RuntimePolicyContext {
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            )
            .await
            .expect("route command");

        assert_eq!(result.status, RuntimeCommandStatus::Deferred);
        let pending = daemon.approval_registry_snapshot().await;
        assert_eq!(pending.approvals.len(), 1);
        let approval_id = pending.approvals[0].approval_id.clone();
        assert_eq!(daemon.metrics().await.pending_approvals, 1);

        let result = daemon
            .route_command(
                RuntimeCommand {
                    command_id: "resolve".to_string(),
                    target: RuntimeCommandTarget {
                        session_id: "session".to_string(),
                        pane_id: None,
                        generation: None,
                    },
                    issued_at_ms: 2,
                    kind: RuntimeCommandKind::ResolveApproval {
                        approval_id,
                        approved: true,
                    },
                },
                RuntimePolicyContext::default(),
            )
            .await
            .expect("resolve approval");

        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        assert_eq!(daemon.approval_registry_snapshot().await.approvals.len(), 0);
        assert_eq!(daemon.metrics().await.pending_approvals, 0);
        let commands = recording_port.commands.lock().await;
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            commands[0].kind,
            RuntimeCommandKind::Interrupt { .. }
        ));
    }

    #[tokio::test]
    async fn daemon_persists_and_restores_pending_approvals() {
        let temp = tempfile::tempdir().expect("tempdir");
        let approval_store_path = temp.path().join("runtime").join("approvals.json");
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            approval_store_path: Some(approval_store_path.clone()),
            approval_store_session_id: Some("session".to_string()),
            ..RuntimeDaemonConfig::default()
        });

        let result = daemon
            .route_command(
                command(RuntimeCommandKind::Interrupt {
                    reason: "operator".to_string(),
                }),
                RuntimePolicyContext {
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            )
            .await
            .expect("route command");

        assert_eq!(result.status, RuntimeCommandStatus::Deferred);
        let persisted: ApprovalStoreSnapshot = serde_json::from_str(
            &std::fs::read_to_string(&approval_store_path).expect("approval store"),
        )
        .expect("decode approval store");
        assert_eq!(persisted.schema_version, APPROVAL_STORE_SCHEMA_VERSION);
        assert_eq!(persisted.session_id.as_deref(), Some("session"));
        assert_eq!(persisted.approvals.len(), 1);
        let approval_id = persisted.approvals[0].approval_id.clone();

        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let restored = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port),
            approval_store_path: Some(approval_store_path.clone()),
            approval_store_session_id: Some("session".to_string()),
            ..RuntimeDaemonConfig::default()
        });
        let report = restored
            .load_persisted_approvals()
            .await
            .expect("load approvals");
        assert_eq!(
            report,
            ApprovalStoreLoadReport {
                loaded: 1,
                ignored_stale: 0,
            }
        );
        assert_eq!(
            restored.approval_registry_snapshot().await.approvals[0].approval_id,
            approval_id
        );

        let result = restored
            .route_command(
                RuntimeCommand {
                    command_id: "resolve".to_string(),
                    target: RuntimeCommandTarget {
                        session_id: "session".to_string(),
                        pane_id: None,
                        generation: None,
                    },
                    issued_at_ms: 2,
                    kind: RuntimeCommandKind::ResolveApproval {
                        approval_id,
                        approved: true,
                    },
                },
                RuntimePolicyContext::default(),
            )
            .await
            .expect("resolve approval");

        assert_eq!(result.status, RuntimeCommandStatus::Applied);
        assert_eq!(recording_port.commands.lock().await.len(), 1);
        let cleared: ApprovalStoreSnapshot = serde_json::from_str(
            &std::fs::read_to_string(&approval_store_path).expect("approval store"),
        )
        .expect("decode cleared approval store");
        assert!(cleared.approvals.is_empty());
    }

    #[tokio::test]
    async fn daemon_clears_stale_persisted_approvals_for_new_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let approval_store_path = temp.path().join("runtime").join("approvals.json");
        let old_daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            approval_store_path: Some(approval_store_path.clone()),
            approval_store_session_id: Some("old-session".to_string()),
            ..RuntimeDaemonConfig::default()
        });
        old_daemon
            .route_command(
                command(RuntimeCommandKind::ResetPane {
                    reason: "operator".to_string(),
                }),
                RuntimePolicyContext {
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            )
            .await
            .expect("route command");

        let (new_daemon, report) =
            RuntimeDaemon::new_with_persisted_approvals(RuntimeDaemonConfig {
                approval_store_path: Some(approval_store_path.clone()),
                approval_store_session_id: Some("new-session".to_string()),
                ..RuntimeDaemonConfig::default()
            })
            .await
            .expect("load new session");

        assert_eq!(
            report,
            ApprovalStoreLoadReport {
                loaded: 0,
                ignored_stale: 1,
            }
        );
        assert!(
            new_daemon
                .approval_registry_snapshot()
                .await
                .approvals
                .is_empty()
        );
        let cleared: ApprovalStoreSnapshot = serde_json::from_str(
            &std::fs::read_to_string(&approval_store_path).expect("approval store"),
        )
        .expect("decode cleared approval store");
        assert_eq!(cleared.session_id.as_deref(), Some("new-session"));
        assert!(cleared.approvals.is_empty());
    }

    #[tokio::test]
    async fn daemon_command_inbox_routes_approval_resolution() {
        let temp = tempfile::tempdir().expect("tempdir");
        let inbox_root = temp.path().join("commands");
        let pending_dir = inbox_root.join("pending");
        let results_dir = inbox_root.join("results");
        tokio::fs::create_dir_all(&pending_dir)
            .await
            .expect("create pending dir");

        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });
        daemon
            .route_command(
                command(RuntimeCommandKind::Interrupt {
                    reason: "operator".to_string(),
                }),
                RuntimePolicyContext {
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            )
            .await
            .expect("route command");
        let approval_id = daemon.approval_registry_snapshot().await.approvals[0]
            .approval_id
            .clone();
        let request = RuntimeCommandInboxRequest {
            request_id: "request-1".to_string(),
            created_at_ms: 1,
            command: RuntimeCommand {
                command_id: "resolve".to_string(),
                target: RuntimeCommandTarget {
                    session_id: "session".to_string(),
                    pane_id: None,
                    generation: None,
                },
                issued_at_ms: 2,
                kind: RuntimeCommandKind::ResolveApproval {
                    approval_id,
                    approved: true,
                },
            },
            context: RuntimePolicyContext::default(),
        };
        tokio::fs::write(
            pending_dir.join("request-1.json"),
            serde_json::to_vec_pretty(&request).expect("encode request"),
        )
        .await
        .expect("write request");

        let processed = process_command_inbox_once(&inbox_root, &daemon)
            .await
            .expect("process inbox");

        assert_eq!(processed, 1);
        assert!(
            daemon
                .approval_registry_snapshot()
                .await
                .approvals
                .is_empty()
        );
        assert_eq!(recording_port.commands.lock().await.len(), 1);
        let result: RuntimeCommandInboxResult = serde_json::from_str(
            &std::fs::read_to_string(results_dir.join("request-1.json")).expect("result file"),
        )
        .expect("decode result");
        assert!(result.error.is_none());
        assert_eq!(
            result.result.expect("command result").status,
            RuntimeCommandStatus::Applied
        );
        assert!(!pending_dir.join("request-1.json").exists());
    }

    #[tokio::test]
    async fn daemon_command_inbox_rejects_stale_session_requests() {
        let temp = tempfile::tempdir().expect("tempdir");
        let inbox_root = temp.path().join("commands");
        let pending_dir = inbox_root.join("pending");
        let results_dir = inbox_root.join("results");
        tokio::fs::create_dir_all(&pending_dir)
            .await
            .expect("create pending dir");

        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            command_port: Some(command_port),
            approval_store_session_id: Some("active-session".to_string()),
            ..RuntimeDaemonConfig::default()
        });
        let request = RuntimeCommandInboxRequest {
            request_id: "stale-request".to_string(),
            created_at_ms: 1,
            command: RuntimeCommand {
                command_id: "interrupt".to_string(),
                target: RuntimeCommandTarget {
                    session_id: "old-session".to_string(),
                    pane_id: Some("worker-1".to_string()),
                    generation: Some(1),
                },
                issued_at_ms: 2,
                kind: RuntimeCommandKind::Interrupt {
                    reason: "operator".to_string(),
                },
            },
            context: RuntimePolicyContext::default(),
        };
        tokio::fs::write(
            pending_dir.join("stale-request.json"),
            serde_json::to_vec_pretty(&request).expect("encode request"),
        )
        .await
        .expect("write request");

        let processed = process_command_inbox_once(&inbox_root, &daemon)
            .await
            .expect("process inbox");

        assert_eq!(processed, 1);
        assert!(recording_port.commands.lock().await.is_empty());
        let result: RuntimeCommandInboxResult = serde_json::from_str(
            &std::fs::read_to_string(results_dir.join("stale-request.json")).expect("result file"),
        )
        .expect("decode result");
        assert_eq!(result.request_id, "stale-request");
        assert!(result.result.is_none());
        assert!(
            result.error.as_deref().is_some_and(
                |error| error.contains("old-session") && error.contains("active-session")
            )
        );
        assert!(!pending_dir.join("stale-request.json").exists());
    }

    #[tokio::test]
    async fn daemon_command_inbox_processes_files_in_stable_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let inbox_root = temp.path().join("commands");
        let pending_dir = inbox_root.join("pending");
        tokio::fs::create_dir_all(&pending_dir)
            .await
            .expect("create pending dir");
        tokio::fs::create_dir_all(pending_dir.join("ignored.json"))
            .await
            .expect("create ignored directory");

        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });

        for (file_name, command_id) in [("b.json", "second"), ("a.json", "first")] {
            let request = RuntimeCommandInboxRequest {
                request_id: command_id.to_string(),
                created_at_ms: 1,
                command: RuntimeCommand {
                    command_id: command_id.to_string(),
                    target: RuntimeCommandTarget {
                        session_id: "session".to_string(),
                        pane_id: Some("worker-1".to_string()),
                        generation: Some(1),
                    },
                    issued_at_ms: 2,
                    kind: RuntimeCommandKind::Interrupt {
                        reason: command_id.to_string(),
                    },
                },
                context: RuntimePolicyContext::default(),
            };
            tokio::fs::write(
                pending_dir.join(file_name),
                serde_json::to_vec_pretty(&request).expect("encode request"),
            )
            .await
            .expect("write request");
        }

        let processed = process_command_inbox_once(&inbox_root, &daemon)
            .await
            .expect("process inbox");

        assert_eq!(processed, 2);
        let commands = recording_port.commands.lock().await;
        let command_ids: Vec<_> = commands
            .iter()
            .map(|command| command.command_id.as_str())
            .collect();
        assert_eq!(command_ids, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn daemon_command_inbox_skips_duplicate_completed_request_ids() {
        let temp = tempfile::tempdir().expect("tempdir");
        let inbox_root = temp.path().join("commands");
        let pending_dir = inbox_root.join("pending");
        let results_dir = inbox_root.join("results");
        tokio::fs::create_dir_all(&pending_dir)
            .await
            .expect("create pending dir");

        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });

        for (file_name, command_id) in [("a.json", "first"), ("b.json", "second")] {
            let request = RuntimeCommandInboxRequest {
                request_id: "duplicate".to_string(),
                created_at_ms: 1,
                command: RuntimeCommand {
                    command_id: command_id.to_string(),
                    target: RuntimeCommandTarget {
                        session_id: "session".to_string(),
                        pane_id: Some("worker-1".to_string()),
                        generation: Some(1),
                    },
                    issued_at_ms: 2,
                    kind: RuntimeCommandKind::Interrupt {
                        reason: command_id.to_string(),
                    },
                },
                context: RuntimePolicyContext::default(),
            };
            tokio::fs::write(
                pending_dir.join(file_name),
                serde_json::to_vec_pretty(&request).expect("encode request"),
            )
            .await
            .expect("write request");
        }

        let processed = process_command_inbox_once(&inbox_root, &daemon)
            .await
            .expect("process inbox");

        assert_eq!(processed, 2);
        let commands = recording_port.commands.lock().await;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command_id, "first");
        drop(commands);
        let result: RuntimeCommandInboxResult = serde_json::from_str(
            &std::fs::read_to_string(results_dir.join("duplicate.json")).expect("result file"),
        )
        .expect("decode result");
        assert_eq!(result.request_id, "duplicate");
        assert_eq!(result.result.expect("command result").command_id, "first");
        assert!(!pending_dir.join("a.json").exists());
        assert!(!pending_dir.join("b.json").exists());
    }

    #[tokio::test]
    async fn daemon_command_inbox_rejects_unsafe_request_ids() {
        let temp = tempfile::tempdir().expect("tempdir");
        let inbox_root = temp.path().join("commands");
        let pending_dir = inbox_root.join("pending");
        let results_dir = inbox_root.join("results");
        tokio::fs::create_dir_all(&pending_dir)
            .await
            .expect("create pending dir");

        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });
        let request = RuntimeCommandInboxRequest {
            request_id: "../bad".to_string(),
            created_at_ms: 1,
            command: RuntimeCommand {
                command_id: "unsafe".to_string(),
                target: RuntimeCommandTarget {
                    session_id: "session".to_string(),
                    pane_id: Some("worker-1".to_string()),
                    generation: Some(1),
                },
                issued_at_ms: 2,
                kind: RuntimeCommandKind::Interrupt {
                    reason: "operator".to_string(),
                },
            },
            context: RuntimePolicyContext::default(),
        };
        tokio::fs::write(
            pending_dir.join("unsafe.json"),
            serde_json::to_vec_pretty(&request).expect("encode request"),
        )
        .await
        .expect("write request");

        let processed = process_command_inbox_once(&inbox_root, &daemon)
            .await
            .expect("process inbox");

        assert_eq!(processed, 1);
        assert!(recording_port.commands.lock().await.is_empty());
        let result: RuntimeCommandInboxResult = serde_json::from_str(
            &std::fs::read_to_string(command_inbox_result_path(&results_dir, "../bad"))
                .expect("result file"),
        )
        .expect("decode result");
        assert_eq!(result.request_id, "../bad");
        assert!(result.result.is_none());
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unsupported file-name characters"))
        );
        assert!(!pending_dir.join("unsafe.json").exists());
    }

    #[tokio::test]
    async fn daemon_denies_pending_approval_without_execution() {
        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });

        daemon
            .route_command(
                command(RuntimeCommandKind::ResetPane {
                    reason: "operator".to_string(),
                }),
                RuntimePolicyContext {
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            )
            .await
            .expect("route command");
        let approval_id = daemon.approval_registry_snapshot().await.approvals[0]
            .approval_id
            .clone();

        let result = daemon
            .route_command(
                RuntimeCommand {
                    command_id: "deny".to_string(),
                    target: RuntimeCommandTarget {
                        session_id: "session".to_string(),
                        pane_id: None,
                        generation: None,
                    },
                    issued_at_ms: 2,
                    kind: RuntimeCommandKind::ResolveApproval {
                        approval_id,
                        approved: false,
                    },
                },
                RuntimePolicyContext::default(),
            )
            .await
            .expect("deny approval");

        assert_eq!(result.status, RuntimeCommandStatus::Rejected);
        assert!(recording_port.commands.lock().await.is_empty());
        assert_eq!(daemon.approval_registry_snapshot().await.approvals.len(), 0);
    }

    #[tokio::test]
    async fn approved_command_rechecks_non_approval_policy() {
        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });

        daemon
            .route_command(
                command(RuntimeCommandKind::SendTerminalInput {
                    bytes: b"unsafe".to_vec(),
                }),
                RuntimePolicyContext {
                    pane_state: Some(RuntimePaneState::Dead),
                    approval_required: true,
                    ..RuntimePolicyContext::default()
                },
            )
            .await
            .expect("route command");
        let approval_id = daemon.approval_registry_snapshot().await.approvals[0]
            .approval_id
            .clone();

        let result = daemon
            .route_command(
                RuntimeCommand {
                    command_id: "approve".to_string(),
                    target: RuntimeCommandTarget {
                        session_id: "session".to_string(),
                        pane_id: None,
                        generation: None,
                    },
                    issued_at_ms: 2,
                    kind: RuntimeCommandKind::ResolveApproval {
                        approval_id,
                        approved: true,
                    },
                },
                RuntimePolicyContext::default(),
            )
            .await
            .expect("approve command");

        assert_eq!(result.status, RuntimeCommandStatus::Rejected);
        assert!(recording_port.commands.lock().await.is_empty());
    }

    #[tokio::test]
    async fn daemon_writes_current_health_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let status_path = temp
            .path()
            .join("runtime")
            .join("daemon")
            .join("current.json");
        let daemon = RuntimeDaemon::default();
        let sidecar_status = RuntimeSidecarStatusHandle::new();

        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ))
            .await
            .expect("publish event");

        RuntimeDaemonHeartbeat::write_current_status(&status_path, &daemon, Some(&sidecar_status))
            .await
            .expect("write heartbeat");

        let status: RuntimeDaemonStatus =
            serde_json::from_str(&std::fs::read_to_string(&status_path).expect("status file"))
                .expect("decode status");
        assert!(status.running);
        assert_eq!(status.metrics.published_events, 1);
        assert_eq!(status.registry_count, 1);
        assert_eq!(
            status.sidecar,
            Some(RuntimeSidecarStatus {
                detection_running: true,
                workflow_running: true,
            })
        );
    }

    #[tokio::test]
    async fn daemon_status_reports_terminal_host_mode() {
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            terminal_host: Some(RuntimeTerminalHostStatus {
                kind: RuntimeTerminalHostKind::Headless,
                experimental: true,
                observation_running: true,
                command_routing: RuntimeTerminalHostCommandRouting::TerminalHost,
                pane_ownership: RuntimeTerminalHostPaneOwnership::Host,
                agent_factory: RuntimeTerminalHostAgentFactoryRouting::Mux,
                capabilities: None,
                promotion_readiness: RuntimeTerminalHostPromotionReadiness::default(),
                session_name: Some("brehon-session".to_string()),
                socket_name: None,
                socket_dir: None,
                binary_path: None,
                diagnostics: Vec::new(),
            }),
            ..RuntimeDaemonConfig::default()
        });

        let status = daemon.status(None).await;

        assert_eq!(
            status.terminal_host,
            Some(RuntimeTerminalHostStatus {
                kind: RuntimeTerminalHostKind::Headless,
                experimental: true,
                observation_running: true,
                command_routing: RuntimeTerminalHostCommandRouting::TerminalHost,
                pane_ownership: RuntimeTerminalHostPaneOwnership::Host,
                agent_factory: RuntimeTerminalHostAgentFactoryRouting::Mux,
                capabilities: None,
                promotion_readiness: RuntimeTerminalHostPromotionReadiness {
                    ready: false,
                    blockers: vec![
                        "worker/reviewer/supervisor factory still mux-owned".to_string(),
                        "terminal-host capabilities are missing".to_string(),
                    ],
                },
                session_name: Some("brehon-session".to_string()),
                socket_name: None,
                socket_dir: None,
                binary_path: None,
                diagnostics: Vec::new(),
            })
        );
    }

    #[test]
    fn terminal_host_promotion_readiness_reports_ready_when_contract_is_met() {
        let status = RuntimeTerminalHostStatus {
            kind: RuntimeTerminalHostKind::Headless,
            experimental: true,
            observation_running: false,
            command_routing: RuntimeTerminalHostCommandRouting::TerminalHost,
            pane_ownership: RuntimeTerminalHostPaneOwnership::Host,
            agent_factory: RuntimeTerminalHostAgentFactoryRouting::TerminalHost,
            capabilities: Some(TerminalHostCapabilities {
                source: RuntimeSource::Headless,
                interactive_pty: false,
                scrollback: true,
                structured_activity: true,
                absolute_resize: true,
                out_of_process_lifecycle: false,
                replay: true,
            }),
            promotion_readiness: RuntimeTerminalHostPromotionReadiness::default(),
            session_name: Some("session".to_string()),
            socket_name: None,
            socket_dir: None,
            binary_path: None,
            diagnostics: Vec::new(),
        };

        let readiness = terminal_host_promotion_readiness(Some(&status));

        assert!(readiness.ready);
        assert!(readiness.blockers.is_empty());
    }

    #[test]
    fn terminal_host_diagnostics_report_bootstrap_and_loss_context() {
        let host = RuntimeTerminalHostStatus {
            kind: RuntimeTerminalHostKind::Headless,
            experimental: true,
            observation_running: true,
            command_routing: RuntimeTerminalHostCommandRouting::TerminalHost,
            pane_ownership: RuntimeTerminalHostPaneOwnership::Host,
            agent_factory: RuntimeTerminalHostAgentFactoryRouting::TerminalHost,
            capabilities: Some(TerminalHostCapabilities {
                source: RuntimeSource::Headless,
                interactive_pty: true,
                scrollback: true,
                structured_activity: true,
                absolute_resize: false,
                out_of_process_lifecycle: true,
                replay: false,
            }),
            promotion_readiness: RuntimeTerminalHostPromotionReadiness::default(),
            session_name: Some("brehon-session".to_string()),
            socket_name: None,
            socket_dir: None,
            binary_path: Some("/tmp/brehon-missing-headless-diagnostic-binary".to_string()),
            diagnostics: Vec::new(),
        };
        let registry = PaneRegistrySnapshot {
            generated_at_ms: 1,
            panes: vec![PaneRegistryEntry {
                session_id: "session".to_string(),
                pane_id: "worker".to_string(),
                generation: 2,
                state: RuntimePaneState::Dead,
                kind: RuntimePaneKind::Worker,
                source: Some(RuntimeSource::Headless),
                title: None,
                last_event_ms: 1,
                last_output_ms: None,
                exit_code: None,
                exit_reason: Some("terminal host disappeared: pane list was empty".to_string()),
            }],
        };

        let diagnostics = terminal_host_diagnostics(Some(&host), &registry, true);
        let codes = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code.as_str())
            .collect::<Vec<_>>();

        assert!(codes.contains(&"terminal_host_binary_missing"));
        assert!(codes.contains(&"terminal_host_absolute_resize_unsupported"));
        assert!(codes.contains(&"terminal_host_session_lost"));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == RuntimeTerminalHostDiagnosticSeverity::Error
                && diagnostic.message.contains("terminal host binary")
        }));
    }

    #[tokio::test]
    async fn daemon_writes_append_only_jsonl_audit_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let audit_log_path = temp.path().join("runtime").join("audit.jsonl");
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            audit_log_path: Some(audit_log_path.clone()),
            ..RuntimeDaemonConfig::default()
        });

        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ))
            .await
            .expect("publish event");

        let contents = std::fs::read_to_string(&audit_log_path).expect("audit log");
        let mut lines = contents.lines();
        let line = lines.next().expect("first audit event");
        assert!(lines.next().is_none());
        let decoded: RuntimeEvent = serde_json::from_str(line).expect("decode audit event");
        assert!(matches!(decoded.kind, RuntimeEventKind::PaneSpawned(_)));
        assert_eq!(daemon.metrics().await.audit_write_errors, 0);
    }

    #[tokio::test]
    async fn audit_log_can_be_read_and_replayed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let audit_log_path = temp.path().join("runtime").join("audit.jsonl");
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            audit_log_path: Some(audit_log_path.clone()),
            ..RuntimeDaemonConfig::default()
        });
        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ))
            .await
            .expect("publish event");

        let events = read_audit_log(&audit_log_path)
            .await
            .expect("read audit log");
        assert_eq!(events.len(), 1);

        let replay_target = RuntimeDaemon::default();
        let replayed = replay_audit_log(&audit_log_path, &replay_target)
            .await
            .expect("replay audit log");
        assert_eq!(replayed, 1);
        assert_eq!(replay_target.pane_registry_snapshot().await.panes.len(), 1);
    }

    #[tokio::test]
    async fn audit_replay_preserves_host_lifecycle_and_generation_fencing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let audit_log_path = temp.path().join("runtime").join("audit.jsonl");
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            audit_log_path: Some(audit_log_path.clone()),
            ..RuntimeDaemonConfig::default()
        });
        let events = vec![
            RuntimeEvent::new(
                meta_with_source("host-pane", 1, RuntimeSource::Headless, 10),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 1, RuntimeSource::Headless, 11),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"before reset".to_vec(),
                    text: Some("before reset".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 1, RuntimeSource::Headless, 12),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: Some(7),
                    reason: Some("terminal host pane disappeared: child exited".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 1, RuntimeSource::Headless, 13),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: None,
                    reason: Some("restart after child exit".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 2, RuntimeSource::Headless, 20),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Worker,
                    title: Some("worker".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 2, RuntimeSource::Headless, 21),
                RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
                    previous: None,
                    current: RuntimePaneState::Ready,
                    reason: Some("terminal host respawn".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 1, RuntimeSource::Headless, 22),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"stale output".to_vec(),
                    text: Some("stale output".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 1, RuntimeSource::Headless, 23),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: Some(99),
                    reason: Some("stale generation exit".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 2, RuntimeSource::Headless, 24),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"after reset".to_vec(),
                    text: Some("after reset".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("host-pane", 2, RuntimeSource::Headless, 25),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: None,
                    reason: Some("terminal host disappeared: no host session running".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("web-pane", 1, RuntimeSource::Web, 30),
                RuntimeEventKind::PaneSpawned(PaneSpawnedEvent {
                    kind: RuntimePaneKind::Reviewer,
                    title: Some("reviewer".to_string()),
                }),
            ),
            RuntimeEvent::new(
                meta_with_source("web-pane", 1, RuntimeSource::Web, 31),
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: None,
                    reason: Some("web terminal host disappeared: pane list was empty".to_string()),
                }),
            ),
        ];

        for event in events.clone() {
            daemon.publish(event).await.expect("publish audit event");
        }

        let audited = read_audit_log(&audit_log_path)
            .await
            .expect("read audit log");
        assert_eq!(audited, events);
        assert!(audited.iter().any(|event| {
            matches!(
                &event.kind,
                RuntimeEventKind::PaneExited(PaneExitedEvent {
                    exit_code: Some(7),
                    reason: Some(reason)
                }) if reason.contains("child exited")
            )
        }));
        assert!(audited.iter().any(|event| {
            event.meta.generation == 1
                && matches!(
                    &event.kind,
                    RuntimeEventKind::PaneExited(PaneExitedEvent {
                        exit_code: Some(99),
                        reason: Some(reason)
                    }) if reason == "stale generation exit"
                )
        }));

        let original_registry = daemon.pane_registry_snapshot().await.panes;
        let replay_target = RuntimeDaemon::default();
        let replayed = replay_audit_log(&audit_log_path, &replay_target)
            .await
            .expect("replay audit log");
        assert_eq!(replayed, events.len());
        let replayed_registry = replay_target.pane_registry_snapshot().await.panes;
        assert_eq!(replayed_registry, original_registry);

        let host_pane = replayed_registry
            .iter()
            .find(|pane| pane.pane_id == "host-pane")
            .expect("host pane replayed");
        assert_eq!(host_pane.generation, 2);
        assert_eq!(host_pane.state, RuntimePaneState::Dead);
        assert_eq!(host_pane.source, Some(RuntimeSource::Headless));
        assert_eq!(host_pane.last_output_ms, Some(24));
        assert_eq!(host_pane.exit_code, None);
        assert_eq!(
            host_pane.exit_reason.as_deref(),
            Some("terminal host disappeared: no host session running")
        );

        let web_pane = replayed_registry
            .iter()
            .find(|pane| pane.pane_id == "web-pane")
            .expect("web pane replayed");
        assert_eq!(web_pane.state, RuntimePaneState::Dead);
        assert_eq!(web_pane.source, Some(RuntimeSource::Web));
        assert_eq!(
            web_pane.exit_reason.as_deref(),
            Some("web terminal host disappeared: pane list was empty")
        );

        let status = replay_target.status(None).await;
        assert_eq!(status.registry.panes, replayed_registry);
        assert_eq!(status.registry_count, 2);
    }

    #[tokio::test]
    async fn sidecar_turns_rate_limit_output_into_detection_and_workflow_audit() {
        let daemon = RuntimeDaemon::default();
        let sidecar = RuntimeSidecar::start_default(daemon.clone());
        let mut rx = daemon.subscribe();

        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"provider rate limit reached".to_vec(),
                    text: None,
                }),
            ))
            .await
            .expect("publish output");

        let mut saw_detection = false;
        let mut saw_workflow = false;
        for _ in 0..6 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.next_event())
                .await
                .expect("sidecar event")
                .expect("runtime stream")
                .expect("runtime event");

            match event.kind {
                RuntimeEventKind::DetectionEvent(detection)
                    if detection.rule_id == "rate_limit.warning" =>
                {
                    saw_detection = true;
                }
                RuntimeEventKind::WorkflowAction(action)
                    if action.workflow_id == "rate_limit.quarantine_recommendation" =>
                {
                    saw_workflow = true;
                }
                _ => {}
            }

            if saw_detection && saw_workflow {
                break;
            }
        }

        sidecar.shutdown().await;

        assert!(saw_detection);
        assert!(saw_workflow);
    }

    #[tokio::test]
    async fn enabled_sidecar_workflow_routes_command_through_daemon() {
        let recording_port = Arc::new(RecordingCommandPort::default());
        let command_port: Arc<dyn RuntimeCommandPort> = recording_port.clone();
        let daemon = RuntimeDaemon::new(RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(command_port),
            ..RuntimeDaemonConfig::default()
        });
        let sidecar = RuntimeSidecar::start(
            daemon.clone(),
            Arc::new(PatternDetectionEngine::default()),
            WorkflowEngine::default()
                .with_enabled_workflows(["rate_limit.quarantine_recommendation"]),
        );
        let mut rx = daemon.subscribe();

        daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: b"provider rate limit reached".to_vec(),
                    text: None,
                }),
            ))
            .await
            .expect("publish output");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut saw_requested_action = false;
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(Some(event))) =
                tokio::time::timeout(std::time::Duration::from_millis(100), rx.next_event()).await
                && matches!(
                    event.kind,
                    RuntimeEventKind::WorkflowAction(ref action)
                        if action.workflow_id == "rate_limit.quarantine_recommendation"
                            && action.status == WorkflowActionStatus::Requested
                            && action.command_id.is_some()
                )
            {
                saw_requested_action = true;
            }

            if saw_requested_action && !recording_port.commands.lock().await.is_empty() {
                break;
            }
        }

        sidecar.shutdown().await;

        assert!(saw_requested_action);
        let commands = recording_port.commands.lock().await;
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            commands[0].kind,
            RuntimeCommandKind::QuarantinePane { .. }
        ));
    }

    #[tokio::test]
    async fn sidecar_status_reports_shutdown() {
        let sidecar = RuntimeSidecar::start_default(RuntimeDaemon::default());
        let status = sidecar.status();
        assert!(status.detection_running);
        assert!(status.workflow_running);
        let handle = sidecar.status_handle();

        sidecar.shutdown().await;

        assert_eq!(
            handle.snapshot(),
            RuntimeSidecarStatus {
                detection_running: false,
                workflow_running: false,
            }
        );
    }

    #[tokio::test]
    async fn daemon_shutdown_rejects_new_events_and_commands() {
        let daemon = RuntimeDaemon::default();
        daemon.shutdown().await;

        let err = daemon
            .publish(RuntimeEvent::new(
                meta("pane", 1),
                RuntimeEventKind::PaneOutput(PaneOutputEvent {
                    bytes: Vec::new(),
                    text: None,
                }),
            ))
            .await
            .expect_err("stopped daemon rejects events");
        assert!(matches!(err, PortError::Runtime(_)));

        let err = daemon
            .route_command(
                command(RuntimeCommandKind::Interrupt {
                    reason: "test".to_string(),
                }),
                RuntimePolicyContext::default(),
            )
            .await
            .expect_err("stopped daemon rejects commands");
        assert!(matches!(err, PortError::Runtime(_)));
    }
}
