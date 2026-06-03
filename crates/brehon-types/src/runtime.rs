//! Runtime event and command types for terminal-host agnostic orchestration.
//!
//! These types are intentionally independent of the existing client/server
//! WebSocket messages. They define the side-channel contract used by the mux,
//! future daemon, semantic detectors, policy gates, and terminal-host adapters.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identity attached to every runtime event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEventMeta {
    /// Optional durable event id assigned by a daemon or store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Brehon session name or daemon session id.
    pub session_id: String,
    /// Pane identifier within the session.
    pub pane_id: String,
    /// Pane generation at the time the event was observed.
    pub generation: u64,
    /// Correlates prompts, commands, policy decisions, and derived events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Component or host that produced the event.
    pub source: RuntimeSource,
}

impl RuntimeEventMeta {
    /// Build event metadata before durable event ids are assigned.
    pub fn new(
        session_id: impl Into<String>,
        pane_id: impl Into<String>,
        generation: u64,
        source: RuntimeSource,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            event_id: None,
            session_id: session_id.into(),
            pane_id: pane_id.into(),
            generation,
            correlation_id: None,
            timestamp_ms,
            source,
        }
    }

    /// Attach a correlation id and return the updated metadata.
    #[must_use]
    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }
}

/// Runtime component or terminal host that produced an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    Mux,
    Daemon,
    EmbeddedTui,
    Web,
    NativeGui,
    Headless,
    Detector,
    Policy,
    Other { name: String },
}

/// Capabilities advertised by a concrete terminal host adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalHostCapabilities {
    pub source: RuntimeSource,
    pub interactive_pty: bool,
    pub scrollback: bool,
    pub structured_activity: bool,
    #[serde(default)]
    pub absolute_resize: bool,
    pub out_of_process_lifecycle: bool,
    pub replay: bool,
}

/// Host-agnostic request to create a terminal pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPaneSpawnSpec {
    pub session_id: String,
    pub pane_id: String,
    pub kind: RuntimePaneKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub rows: u16,
    pub cols: u16,
}

/// Stable pane identity returned by a terminal host adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPaneHandle {
    pub session_id: String,
    pub pane_id: String,
    pub generation: u64,
    pub source: RuntimeSource,
}

/// Durable snapshot of the assignment context currently loaded in a pane.
///
/// The mux mirrors task/review context changes into runtime files so MCP tools
/// can authoritatively answer whether a worker or reviewer pane is showing the
/// expected assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAssignmentContext {
    pub pane_id: String,
    pub assignment_kind: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    pub status: String,
    pub updated_at: String,
}

const PANE_ASSIGNMENT_CONTEXT_DIRNAME: &str = "pane-assignment-context";

/// Sanitize a runtime identifier for use in filenames.
pub fn sanitize_runtime_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Scan `dir` for a finalized `.json` ack file whose JSON payload contains the
/// given `prompt_id`. Returns the matching file path together with the parsed
/// JSON value so callers can avoid a second parse.
///
/// This avoids the collision risk of filename-based sanitization by matching on
/// file content, consistent with how queue and dead-letter directories work.
///
/// Performance: O(n) in directory entries. Ack directories are expected to
/// remain small (single-digit file counts per session), so this scan is cheap
/// in practice and eliminates the collision risk of the old O(1) filename-
/// based lookup.
pub fn find_prompt_ack_in_dir(dir: &Path, prompt_id: &str) -> Option<(PathBuf, Value)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<Value>(&content) {
                if value.get("prompt_id").and_then(|v| v.as_str()) == Some(prompt_id) {
                    return Some((path, value));
                }
            }
        }
    }
    None
}

/// Return the runtime directory used for pane assignment context snapshots.
pub fn pane_assignment_context_dir(root: &Path) -> PathBuf {
    root.join("runtime").join(PANE_ASSIGNMENT_CONTEXT_DIRNAME)
}

/// Return the snapshot path for a pane assignment context file.
pub fn pane_assignment_context_path(root: &Path, pane_id: &str) -> PathBuf {
    let pane_key = sanitize_runtime_key(pane_id);
    let pane_key = if pane_key.is_empty() {
        "pane"
    } else {
        pane_key.as_str()
    };
    pane_assignment_context_dir(root).join(format!("{pane_key}.json"))
}

/// Requested terminal dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalResize {
    pub rows: u16,
    pub cols: u16,
}

/// Runtime event envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEvent {
    pub meta: RuntimeEventMeta,
    pub kind: RuntimeEventKind,
}

impl RuntimeEvent {
    pub fn new(meta: RuntimeEventMeta, kind: RuntimeEventKind) -> Self {
        Self { meta, kind }
    }
}

/// Events emitted by muxes, host adapters, detectors, policies, and daemons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEventKind {
    PaneOutput(PaneOutputEvent),
    PaneStateChanged(PaneStateChangedEvent),
    PromptQueued(PromptQueuedEvent),
    PromptDelivered(PromptDeliveredEvent),
    PromptRejected(PromptRejectedEvent),
    AgentTurnStarted(AgentTurnEvent),
    AgentTurnEnded(AgentTurnEvent),
    ActivityObserved(ActivityObservedEvent),
    DetectionEvent(DetectionEvent),
    PolicyDecision(PolicyDecisionEvent),
    WorkflowAction(WorkflowActionEvent),
    PaneSpawned(PaneSpawnedEvent),
    PaneExited(PaneExitedEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneOutputEvent {
    /// Raw terminal bytes when available.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bytes: Vec<u8>,
    /// Normalized text suitable for detection and transcript views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneStateChangedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<RuntimePaneState>,
    pub current: RuntimePaneState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<RuntimePaneBlockInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePaneState {
    Ready,
    Busy,
    Blocked,
    Dead,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePaneBlockKind {
    PermissionRequest,
    TerminalPrompt,
    ProviderContextLimit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePaneBlockInfo {
    pub kind: RuntimePaneBlockKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_or_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptQueuedEvent {
    pub prompt_id: String,
    pub queue_depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptDeliveredEvent {
    pub prompt_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRejectedEvent {
    pub prompt_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTurnEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityObservedEvent {
    pub kind: RuntimeActivityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeActivityKind {
    Output,
    ToolStarted,
    ToolCompleted,
    OperationStarted,
    OperationCompleted,
    PermissionRequested,
    PermissionResolved,
    Heartbeat,
    Other { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionEvent {
    pub detection_id: String,
    pub rule_id: String,
    pub severity: DetectionSeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<RuntimeTextSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionSeverity {
    Info,
    Warning,
    Blocking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeTextSpan {
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionEvent {
    pub decision_id: String,
    pub operation: RuntimeOperation,
    pub decision: RuntimePolicyDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowActionEvent {
    pub workflow_id: String,
    pub action_id: String,
    pub operation: RuntimeOperation,
    pub status: WorkflowActionStatus,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowActionStatus {
    DryRun,
    Requested,
    Applied,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOperation {
    SendPrompt,
    BroadcastPrompt,
    SendTerminalInput,
    Interrupt,
    ResetPane,
    RecyclePane,
    QuarantinePane,
    SpawnPane,
    ResizePane,
    ClosePane,
    ResolveApproval,
    Other { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RuntimePolicyDecision {
    Allow,
    Deny { reason: String },
    Defer { retry_after_ms: u64, reason: String },
    RequireApproval { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePolicyRequest {
    pub command: RuntimeCommand,
    #[serde(default)]
    pub context: RuntimePolicyContext,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePolicyContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_state: Option<RuntimePaneState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_prompts: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcast_fanout: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_failures: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited_until_ms: Option<u64>,
    #[serde(default)]
    pub approval_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSpawnedEvent {
    pub kind: RuntimePaneKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneExitedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePaneKind {
    Supervisor,
    Worker,
    Reviewer,
    Advisor,
    Research,
    Director,
    Shell,
    Unknown,
    Other { name: String },
}

/// Command envelope for mutating runtime operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCommand {
    pub command_id: String,
    pub target: RuntimeCommandTarget,
    pub issued_at_ms: u64,
    pub kind: RuntimeCommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCommandTarget {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeCommandKind {
    SendPrompt {
        prompt_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        delivery: PromptDeliveryMode,
    },
    BroadcastPrompt {
        prompt_id: String,
        text: String,
        pane_ids: Vec<String>,
    },
    SendTerminalInput {
        bytes: Vec<u8>,
    },
    Interrupt {
        reason: String,
    },
    ResetPane {
        reason: String,
    },
    RecyclePane {
        reason: String,
    },
    QuarantinePane {
        reason: String,
    },
    SpawnPane {
        kind: RuntimePaneKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cols: Option<u16>,
    },
    ResizePane {
        rows: u16,
        cols: u16,
    },
    ClosePane {
        reason: String,
    },
    ResolveApproval {
        approval_id: String,
        approved: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDeliveryMode {
    /// Try one delivery attempt and report deferred when the transport is not ready.
    Attempt,
    /// Queue behind any current turn and deliver when the pane is ready.
    Enqueue,
    /// Deliver immediately only when policy and state say direct input is safe.
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCommandResult {
    pub command_id: String,
    pub status: RuntimeCommandStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCommandStatus {
    Accepted,
    Applied,
    Rejected,
    Deferred,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn runtime_event_round_trips_json() {
        let meta = RuntimeEventMeta::new("session-1", "pane-1", 7, RuntimeSource::Mux, 1234)
            .with_correlation_id("prompt-1");
        let event = RuntimeEvent::new(
            meta,
            RuntimeEventKind::PromptDelivered(PromptDeliveredEvent {
                prompt_id: "prompt-1".to_string(),
            }),
        );

        let encoded = serde_json::to_string(&event).expect("serialize runtime event");
        let decoded: RuntimeEvent =
            serde_json::from_str(&encoded).expect("deserialize runtime event");

        assert_eq!(decoded, event);
    }

    #[test]
    fn runtime_command_round_trips_json() {
        let mut spawn_env = BTreeMap::new();
        spawn_env.insert("BREHON_SMOKE".to_string(), "1".to_string());
        let command = RuntimeCommand {
            command_id: "cmd-1".to_string(),
            target: RuntimeCommandTarget {
                session_id: "session-1".to_string(),
                pane_id: Some("pane-1".to_string()),
                generation: Some(2),
            },
            issued_at_ms: 5678,
            kind: RuntimeCommandKind::SpawnPane {
                kind: RuntimePaneKind::Worker,
                pane_id: Some("pane-1".to_string()),
                title: Some("worker".to_string()),
                cwd: Some("/workspace".to_string()),
                command: vec!["cat".to_string()],
                env: spawn_env,
                rows: Some(30),
                cols: Some(100),
            },
        };

        let encoded = serde_json::to_string(&command).expect("serialize runtime command");
        let decoded: RuntimeCommand =
            serde_json::from_str(&encoded).expect("deserialize runtime command");

        assert_eq!(decoded, command);
        let RuntimeCommandKind::SpawnPane { rows, cols, .. } = decoded.kind else {
            panic!("expected spawn command");
        };
        assert_eq!(rows, Some(30));
        assert_eq!(cols, Some(100));
    }

    #[test]
    fn terminal_host_types_round_trip_json() {
        let mut env = BTreeMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        let spec = TerminalPaneSpawnSpec {
            session_id: "session-1".to_string(),
            pane_id: "pane-1".to_string(),
            kind: RuntimePaneKind::Worker,
            title: Some("worker".to_string()),
            cwd: Some("/workspace".to_string()),
            command: vec!["codex".to_string()],
            env,
            rows: 40,
            cols: 120,
        };
        let handle = TerminalPaneHandle {
            session_id: spec.session_id.clone(),
            pane_id: spec.pane_id.clone(),
            generation: 1,
            source: RuntimeSource::Headless,
        };
        let resize = TerminalResize {
            rows: 50,
            cols: 160,
        };

        let decoded_spec: TerminalPaneSpawnSpec =
            serde_json::from_str(&serde_json::to_string(&spec).expect("serialize spec"))
                .expect("deserialize spec");
        let decoded_handle: TerminalPaneHandle =
            serde_json::from_str(&serde_json::to_string(&handle).expect("serialize handle"))
                .expect("deserialize handle");
        let decoded_resize: TerminalResize =
            serde_json::from_str(&serde_json::to_string(&resize).expect("serialize resize"))
                .expect("deserialize resize");

        assert_eq!(decoded_spec, spec);
        assert_eq!(decoded_handle, handle);
        assert_eq!(decoded_resize, resize);
    }

    #[test]
    fn sanitize_runtime_key_replaces_non_filename_chars() {
        assert_eq!(sanitize_runtime_key("pane 1/2:3"), "pane_1_2_3");
        assert_eq!(sanitize_runtime_key("worker-1_ok"), "worker-1_ok");
    }

    #[test]
    fn pane_assignment_context_path_uses_runtime_snapshot_dir() {
        let path = pane_assignment_context_path(Path::new("/tmp/brehon"), "pane 1/2");
        assert_eq!(
            path,
            Path::new("/tmp/brehon")
                .join("runtime")
                .join("pane-assignment-context")
                .join("pane_1_2.json")
        );
    }

    #[test]
    fn pane_assignment_context_path_falls_back_for_empty_pane_ids() {
        let path = pane_assignment_context_path(Path::new("/tmp/brehon"), "");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("pane.json")
        );
    }
}
