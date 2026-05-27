//! SDK for Brehon agent adapters.
//!
//! This crate defines the [`AgentAdapter`] trait that every per-CLI adapter
//! crate must implement. It is the single seam that [`brehon-acp`](../../brehon_acp)
//! and [`brehon-mux`](../../brehon_mux) use to dispatch to concrete transports
//! without knowing their internals.
//!
//! # Architecture
//!
//! Each adapter crate (e.g. `brehon-adapter-codex`, `brehon-adapter-gemini`)
//! depends **only** on:
//!
//! - `brehon-adapter-sdk`
//! - `brehon-types`
//! - `brehon-ports`
//!
//! `brehon-acp` depends on all adapter crates (dependency inversion).
//!
//! # Shared Infrastructure
//!
//! This crate also hosts shared infrastructure previously in `brehon-acp`:
//!
//! - [`process`] — subprocess management (`AgentProcess`)
//! - [`protocol`] — JSON-RPC 2.0 message types
//! - [`session_event`] — session event normalization (`SessionEvent`)
//! - [`stability_runtime`] — session stability snapshot persistence

pub mod direct_tools;
pub mod harness;
#[cfg(feature = "process")]
pub mod process;
pub mod protocol;
pub mod session_event;
pub mod stability_runtime;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use brehon_types::{
    AdapterKind, AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId,
    SessionInfo, SessionSpec, TerminalId,
};
pub use harness::{
    HarnessCapabilities, HarnessControlPlane, HarnessTransport, PromptInjectionStrategy,
    SupervisorCli,
};
#[cfg(feature = "process")]
pub use process::AgentProcess;
pub use protocol::{
    JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId,
};
pub use session_event::{
    normalize_session_update_value, session_event_to_domain_event, SessionEvent, UpdateError,
};
pub use stability_runtime::{
    clear_session_snapshot, persist_session_snapshot, schedule_clear_session_snapshot,
    schedule_persist_session_snapshot,
};

/// Classification of adapter-level failures.
///
/// Each variant corresponds to a distinct operational failure mode so that
/// callers can match on the error kind without parsing human-readable text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterErrorKind {
    /// The agent process or transport could not be started.
    SpawnFailed,
    /// A message could not be sent to the agent.
    SendFailed,
    /// An operation exceeded its allotted time.
    TimedOut,
    /// The requested operation is not supported by this adapter.
    UnsupportedOperation,
    /// The underlying transport closed unexpectedly.
    TransportClosed,
}

/// Error produced by an adapter operation.
///
/// Carries a structured [`AdapterErrorKind`] so callers can react
/// programmatically, plus a human-readable message for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterError {
    /// Classification of what went wrong.
    pub kind: AdapterErrorKind,
    /// Human-readable description of what went wrong.
    pub message: String,
}

impl AdapterError {
    /// Create a new adapter error with the given kind and message.
    pub fn new(kind: AdapterErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Convenience constructor for [`AdapterErrorKind::SpawnFailed`].
    pub fn spawn_failed(message: impl Into<String>) -> Self {
        Self::new(AdapterErrorKind::SpawnFailed, message)
    }

    /// Convenience constructor for [`AdapterErrorKind::SendFailed`].
    pub fn send_failed(message: impl Into<String>) -> Self {
        Self::new(AdapterErrorKind::SendFailed, message)
    }

    /// Convenience constructor for [`AdapterErrorKind::TimedOut`].
    pub fn timed_out(message: impl Into<String>) -> Self {
        Self::new(AdapterErrorKind::TimedOut, message)
    }

    /// Convenience constructor for [`AdapterErrorKind::UnsupportedOperation`].
    pub fn unsupported_operation(message: impl Into<String>) -> Self {
        Self::new(AdapterErrorKind::UnsupportedOperation, message)
    }

    /// Convenience constructor for [`AdapterErrorKind::TransportClosed`].
    pub fn transport_closed(message: impl Into<String>) -> Self {
        Self::new(AdapterErrorKind::TransportClosed, message)
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for AdapterError {}

/// Result type alias for adapter operations.
pub type AdapterResult<T> = Result<T, AdapterError>;

/// Result of a completed prompt turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PromptResult {
    /// Text response from the agent, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Token usage reported by the agent, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    /// Stop reason (e.g. "stop", "length", "tool_calls"), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

/// Events emitted by an adapter session.
///
/// This is the adapter SDK's canonical event type. Each concrete adapter
/// normalizes its transport-specific events into this shape so that the
/// gateway and mux can consume them uniformly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdapterEvent {
    /// Streamed text output from the agent.
    Output {
        /// The text content.
        text: String,
    },
    /// A long-running operation has started.
    OperationStarted {
        /// Name or description of the operation.
        operation: String,
    },
    /// A long-running operation has completed.
    OperationCompleted {
        /// Name or description of the operation.
        operation: String,
        /// Whether the operation succeeded.
        success: bool,
    },
    /// The agent is requesting a permission grant.
    PermissionRequest {
        /// Identifier for this permission request.
        permission_id: String,
        /// Action the agent wants permission for.
        action: String,
        /// Additional details (options, scope, etc.).
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    /// Incremental progress update from the agent.
    Progress {
        /// Human-readable progress message.
        message: String,
        /// Optional completion percentage (0–100).
        #[serde(skip_serializing_if = "Option::is_none")]
        percent: Option<u8>,
    },
    /// A tool invocation has begun.
    ToolCallStarted {
        /// Tool call identifier.
        tool_id: String,
        /// Tool name.
        tool_name: String,
        /// Optional normalized tool input/details.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    /// A tool invocation has finished.
    ToolCallCompleted {
        /// Tool call identifier.
        tool_id: String,
        /// Tool name.
        tool_name: String,
        /// Final status (e.g. "completed", "failed").
        status: String,
        /// Optional normalized tool output/details.
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
}

/// Trait implemented by every per-CLI adapter crate.
///
/// The trait abstracts the lifecycle of a single agent session. An
/// [`AgentAdapter`] instance represents one active (or activatable) session;
/// `spawn` initiates the session and the remaining methods operate on it.
///
/// All methods are `Send + Sync` so that adapters can be held in
/// `Arc<dyn AgentAdapter>` registries and accessed concurrently.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Spawn the agent session according to the provided specification.
    ///
    /// Returns the [`SessionId`] that uniquely identifies this session
    /// within the adapter.
    async fn spawn(&self, spec: SessionSpec) -> AdapterResult<SessionId>;

    /// Send a prompt turn to the active session.
    ///
    /// Returns a [`PromptHandle`] that can be used to track or cancel
    /// the in-flight prompt.
    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle>;

    /// Block until the response for `prompt_id` arrives or `timeout_ms` elapses.
    ///
    /// Returns the [`PromptResult`] on success. If the timeout expires
    /// before a response is received, returns [`AdapterError`] with a
    /// timeout message.
    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult>;

    /// Obtain a channel receiver for events emitted by this session.
    ///
    /// Each call returns a fresh [`mpsc::Receiver`] that will receive
    /// [`AdapterEvent`]s from this point forward. The exact behaviour
    /// when called multiple times is adapter-defined (some adapters may
    /// return an error on a second call, others may broadcast to all
    /// receivers).
    fn events(&self) -> mpsc::Receiver<AdapterEvent>;

    /// Terminate the session.
    ///
    /// Implementations should attempt graceful shutdown first and fall
    /// back to forceful termination if the adapter does not respond.
    async fn terminate(&self) -> AdapterResult<()>;

    /// Return the [`AdapterKind`] for this adapter.
    fn kind(&self) -> AdapterKind;

    /// Return the capabilities of this session.
    ///
    /// Capabilities are typically negotiated during `spawn`, so this
    /// method may return cached values rather than re-query the agent.
    async fn capabilities(&self) -> AdapterResult<AgentCapabilities>;

    /// Return the [`SessionId`] for this active session.
    async fn session_id(&self) -> SessionId;

    /// Return [`SessionInfo`] for this session.
    async fn session_info(&self) -> SessionInfo;

    /// Return stability counters for this session.
    async fn stability_counters(&self) -> brehon_types::StabilityCounters;

    /// Set a configuration option on the session.
    async fn set_config(&self, option: &str, value: &str) -> AdapterResult<()>;

    /// Cancel an in-flight prompt.
    async fn cancel_prompt(&self, prompt: &PromptId) -> AdapterResult<()>;

    /// Check the health of the session.
    async fn health_check(&self) -> AdapterResult<HealthStatus>;

    /// Attach an interactive terminal to the session.
    ///
    /// Returns `Ok(Some(terminal_id))` if terminal support is available.
    /// Returns `Ok(None)` if the agent does not support terminals.
    async fn attach_terminal(&self, _cols: u16, _rows: u16) -> AdapterResult<Option<TerminalId>> {
        Ok(None)
    }

    /// Send input to an attached terminal.
    async fn send_terminal_input(
        &self,
        _terminal: &TerminalId,
        _input: Vec<u8>,
    ) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Terminal input is not supported for this adapter",
        ))
    }

    /// Resolve a pending permission request.
    async fn resolve_permission(&self, _permission_id: &str, _approved: bool) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Permission resolution is not supported for this adapter",
        ))
    }

    /// Return a `&dyn Any` for downcasting to concrete adapter types.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Prepend the directory containing the current Brehon executable to `PATH`.
///
/// This ensures that spawned agents can find the `brehon` binary
/// for MCP registration even when the host shell's PATH is minimal.
pub fn prepend_current_exe_dir_to_path(env: &mut Vec<(String, String)>) {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|p| p.to_path_buf()))
    else {
        return;
    };

    let path_value = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut paths = vec![exe_dir];
            paths.extend(std::env::split_paths(&existing));
            std::env::join_paths(paths).ok()
        }
        None => std::env::join_paths([exe_dir]).ok(),
    };

    if let Some(path_value) = path_value {
        env.push(("PATH".to_string(), path_value.to_string_lossy().to_string()));
    }
}

/// Push `BREHON_ROOT` and, when resolvable, `BREHON_PROJECT_ROOT` into the
/// environment of a spawned agent subprocess.
///
/// The `BREHON_ROOT` convention points at the `.brehon/` directory; the project
/// root is its parent. Exposing both keys explicitly avoids a class of silent
/// bugs where downstream consumers needed the project root and inferred it
/// from `BREHON_ROOT` (or silently fell back to defaults when they couldn't).
///
/// **Why this helper exists:** it replaced per-adapter ad-hoc blocks where
/// codex/gemini/opencode only pushed `BREHON_ROOT` while claude/kimi pushed
/// both. The inconsistency silently disabled `share_after_submit` reviewer
/// resets for codex reviewers under some configurations. Routing every
/// adapter through this helper ensures future additions can't repeat the
/// mistake.
pub fn push_brehon_root_env(env: &mut Vec<(String, String)>, brehon_root: &std::path::Path) {
    env.push((
        "BREHON_ROOT".to_string(),
        brehon_root.to_string_lossy().to_string(),
    ));
    if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        if let Some(project_root) = brehon_root.parent() {
            if !project_root.as_os_str().is_empty() {
                env.push((
                    "BREHON_PROJECT_ROOT".to_string(),
                    project_root.to_string_lossy().to_string(),
                ));
            }
        }
    }
}

/// Push `BREHON_WORKSPACE_ROOT` and optionally `BREHON_WORKTREE_BRANCH`
/// into the environment.
pub fn push_workspace_root_env(env: &mut Vec<(String, String)>, cwd: &std::path::Path) {
    env.push((
        "BREHON_WORKSPACE_ROOT".to_string(),
        cwd.to_string_lossy().to_string(),
    ));

    let output = match std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return,
    };

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !branch.is_empty() {
        env.push(("BREHON_WORKTREE_BRANCH".to_string(), branch));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_error_display() {
        let err = AdapterError::spawn_failed("process exited with code 1");
        assert_eq!(err.to_string(), "SpawnFailed: process exited with code 1");
        assert_eq!(err.kind, AdapterErrorKind::SpawnFailed);
    }

    #[test]
    fn adapter_error_kind_variants() {
        let kinds = vec![
            (
                AdapterError::spawn_failed("x"),
                AdapterErrorKind::SpawnFailed,
            ),
            (AdapterError::send_failed("x"), AdapterErrorKind::SendFailed),
            (AdapterError::timed_out("x"), AdapterErrorKind::TimedOut),
            (
                AdapterError::unsupported_operation("x"),
                AdapterErrorKind::UnsupportedOperation,
            ),
            (
                AdapterError::transport_closed("x"),
                AdapterErrorKind::TransportClosed,
            ),
        ];
        for (err, expected) in kinds {
            assert_eq!(err.kind, expected);
        }
    }

    #[test]
    fn prompt_result_roundtrip() {
        let result = PromptResult {
            response: Some("hello".into()),
            tokens_used: Some(42),
            stop_reason: Some("stop".into()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: PromptResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, parsed);
    }

    #[test]
    fn adapter_event_roundtrip() {
        let event = AdapterEvent::Progress {
            message: "50% done".into(),
            percent: Some(50),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AdapterEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn push_brehon_root_env_emits_project_root_when_brehon_root_is_dot_brehon() {
        let mut env = Vec::new();
        push_brehon_root_env(&mut env, std::path::Path::new("/repo/.brehon"));
        assert!(env.contains(&("BREHON_ROOT".to_string(), "/repo/.brehon".to_string())));
        assert!(env.contains(&("BREHON_PROJECT_ROOT".to_string(), "/repo".to_string())));
    }

    #[test]
    fn push_brehon_root_env_omits_project_root_for_non_dot_brehon_root() {
        let mut env = Vec::new();
        push_brehon_root_env(&mut env, std::path::Path::new("/custom/state"));
        assert!(env.contains(&("BREHON_ROOT".to_string(), "/custom/state".to_string())));
        assert!(env.iter().all(|(key, _)| key != "BREHON_PROJECT_ROOT"));
    }

    #[test]
    fn push_brehon_root_env_handles_dot_brehon_at_filesystem_root() {
        let mut env = Vec::new();
        // If `.brehon` has no parent (e.g. it IS the root, `/`), don't emit
        // a nonsensical empty BREHON_PROJECT_ROOT.
        push_brehon_root_env(&mut env, std::path::Path::new("/.brehon"));
        assert!(env.contains(&("BREHON_ROOT".to_string(), "/.brehon".to_string())));
        // Parent of `/.brehon` is `/`, which we DO emit (it's a valid path).
        // This test locks in the current contract: we defer to Path::parent
        // and only skip when it returns None. The only time it returns None
        // is for literal path roots or relative paths without a parent.
        let project_root_emitted = env.iter().any(|(key, _)| key == "BREHON_PROJECT_ROOT");
        assert!(project_root_emitted);
    }

    #[test]
    fn push_brehon_root_env_skips_empty_project_root() {
        let mut env = Vec::new();
        // `.brehon` alone has `Some("")` as its parent. Emitting
        // `BREHON_PROJECT_ROOT=""` would be worse than not emitting it, since
        // consumers that treat the var as authoritative would then resolve
        // paths relative to the process cwd in surprising ways.
        push_brehon_root_env(&mut env, std::path::Path::new(".brehon"));
        assert!(env.contains(&("BREHON_ROOT".to_string(), ".brehon".to_string())));
        assert!(env.iter().all(|(key, _)| key != "BREHON_PROJECT_ROOT"));
    }
}
