use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brehon_daemon::{
    ApprovalStoreSnapshot, PendingApprovalEntry, RuntimeCommandInboxRequest,
    RuntimeCommandInboxResult, RuntimeDaemonStatus, RuntimeTerminalHostAgentFactoryRouting,
    RuntimeTerminalHostCommandRouting, RuntimeTerminalHostStatus,
};
use brehon_types::{
    RuntimeCommand, RuntimeCommandKind, RuntimeCommandTarget, RuntimePaneState,
    RuntimePolicyContext, RuntimeSource, RuntimeTerminalHostKind, RuntimeTerminalHostPaneOwnership,
};
use anyhow::{bail, Context, Result};

const STALE_RUNTIME_STATUS_AFTER_MS: u64 = 15_000;

pub async fn status(project_path: Option<&Path>, json: bool) -> Result<()> {
    let path = daemon_status_path(project_path);
    let status = read_status(&path).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    print!("{}", format_status_text(&status));
    Ok(())
}

pub async fn dashboard(project_path: Option<&Path>) -> Result<()> {
    let project_root = project_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let brehon_root = project_root.join(".brehon");
    let shutdown = Arc::new(AtomicBool::new(false));
    crate::signals::setup_signal_handlers(shutdown.clone())?;
    let rt = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || brehon_tui::run_dashboard_tui(shutdown, rt, brehon_root))
        .await
        .context("dashboard TUI task failed")?
        .context("dashboard TUI failed")?;
    Ok(())
}

fn format_status_text(status: &RuntimeDaemonStatus) -> String {
    format_status_text_at(status, unix_timestamp_ms())
}

fn format_status_text_at(status: &RuntimeDaemonStatus, now_ms: u64) -> String {
    let mut output = String::new();
    let heartbeat_age_ms = status_heartbeat_age_ms(status, now_ms);
    let stale = status_is_stale(status, now_ms);
    let daemon_state = if stale {
        "stale"
    } else if status.running {
        "running"
    } else {
        "stopped"
    };
    let _ = writeln!(output, "runtime daemon: {}", daemon_state);
    let _ = writeln!(output, "heartbeat_age_ms: {heartbeat_age_ms}");
    if stale {
        let _ = writeln!(output, "reported_running: true");
    }
    let _ = writeln!(output, "uptime_ms: {}", status.uptime_ms);
    let _ = writeln!(output, "panes: {}", status.registry_count);
    let source_summary = registry_source_summary(status);
    if !source_summary.is_empty() {
        let _ = writeln!(output, "pane_sources: {source_summary}");
    }
    let _ = writeln!(
        output,
        "pending_approvals: {}",
        status.metrics.pending_approvals
    );
    let _ = writeln!(
        output,
        "published_events: {}",
        status.metrics.published_events
    );
    let _ = writeln!(
        output,
        "routed_commands: {}",
        status.metrics.routed_commands
    );
    if let Some(host) = status.terminal_host.as_ref() {
        let _ = writeln!(
            output,
            "terminal_host: kind={:?} mode={} experimental={} observation={} commands={:?} pane_owner={} agent_factory={} resize={} host_panes={}",
            host.kind,
            terminal_host_mode_label(status, host),
            host.experimental,
            host.observation_running,
            host.command_routing,
            terminal_host_pane_ownership_label(host.pane_ownership),
            terminal_host_agent_factory_label(host.agent_factory),
            terminal_host_resize_label(host),
            terminal_host_pane_count(status, host.kind)
        );
        if let Some(session_name) = host.session_name.as_deref() {
            let _ = writeln!(output, "terminal_host_session: {session_name}");
        }
        if let Some(socket_name) = host.socket_name.as_deref() {
            let _ = writeln!(output, "terminal_host_socket: {socket_name}");
        }
        if let Some(socket_dir) = host.socket_dir.as_deref() {
            let _ = writeln!(output, "terminal_host_socket_dir: {socket_dir}");
        }
        if let Some(binary_path) = host.binary_path.as_deref() {
            let _ = writeln!(output, "terminal_host_binary: {binary_path}");
        }
        if let Some(capabilities) = host.capabilities.as_ref() {
            let _ = writeln!(
                output,
                "terminal_host_capabilities: {}",
                terminal_host_capabilities_summary(capabilities)
            );
        }
        let promotion_readiness = brehon_daemon::terminal_host_promotion_readiness(Some(host));
        let _ = writeln!(
            output,
            "terminal_host_promotion: {} blockers={}",
            if promotion_readiness.ready {
                "ready"
            } else {
                "blocked"
            },
            promotion_readiness.blockers.len()
        );
        for blocker in &promotion_readiness.blockers {
            let _ = writeln!(output, "terminal_host_promotion_blocker: {blocker}");
        }
        for diagnostic in &host.diagnostics {
            let _ = writeln!(
                output,
                "terminal_host_diagnostic: severity={} code={} message={}",
                terminal_host_diagnostic_severity_label(diagnostic.severity),
                diagnostic.code,
                diagnostic.message
            );
            if let Some(action) = diagnostic.action.as_deref() {
                let _ = writeln!(output, "terminal_host_diagnostic_action: {action}");
            }
        }
        if let Some(attach_command) = terminal_host_attach_command(host) {
            let _ = writeln!(output, "terminal_host_attach: {attach_command}");
        }
    }
    if let Some(sidecar) = status.sidecar {
        let _ = writeln!(
            output,
            "sidecar: detection={} workflow={}",
            sidecar.detection_running, sidecar.workflow_running
        );
    }
    if let Some(path) = status.audit_log_path.as_deref() {
        let _ = writeln!(output, "audit_log: {path}");
    }
    if let Some(path) = status.approval_store_path.as_deref() {
        let _ = writeln!(output, "approval_store: {path}");
    }
    if !status.registry.panes.is_empty() {
        let _ = writeln!(output, "registry:");
        for pane in &status.registry.panes {
            let _ = write!(
                output,
                "  {}/{} gen={} state={:?} kind={:?}",
                pane.session_id, pane.pane_id, pane.generation, pane.state, pane.kind
            );
            if let Some(source) = pane.source.as_ref() {
                let _ = write!(output, " source={source:?}");
            }
            if let Some(title) = pane.title.as_deref() {
                let _ = write!(output, " title={title:?}");
            }
            if let Some(last_output_ms) = pane.last_output_ms {
                let _ = write!(output, " last_output_ms={last_output_ms}");
            }
            if let Some(exit_code) = pane.exit_code {
                let _ = write!(output, " exit_code={exit_code}");
            }
            if let Some(reason) = pane.exit_reason.as_deref() {
                let _ = write!(output, " exit_reason={reason:?}");
            }
            let _ = writeln!(output);
        }
    }
    output
}

fn registry_source_summary(status: &RuntimeDaemonStatus) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for pane in &status.registry.panes {
        let source = pane
            .source
            .as_ref()
            .map(|source| format!("{source:?}"))
            .unwrap_or_else(|| "Unknown".to_string());
        *counts.entry(source).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(source, count)| format!("{source}={count}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn terminal_host_mode_label(
    status: &RuntimeDaemonStatus,
    host: &RuntimeTerminalHostStatus,
) -> &'static str {
    if host.kind == RuntimeTerminalHostKind::Embedded {
        return "embedded";
    }
    if host.command_routing == RuntimeTerminalHostCommandRouting::TerminalHost {
        return "host-owned";
    }
    if terminal_host_pane_count(status, host.kind) > 0 {
        return "preview";
    }
    "standby"
}

fn terminal_host_pane_ownership_label(ownership: RuntimeTerminalHostPaneOwnership) -> &'static str {
    match ownership {
        RuntimeTerminalHostPaneOwnership::Mux => "mux",
        RuntimeTerminalHostPaneOwnership::Host => "host",
    }
}

fn terminal_host_agent_factory_label(
    routing: RuntimeTerminalHostAgentFactoryRouting,
) -> &'static str {
    match routing {
        RuntimeTerminalHostAgentFactoryRouting::Mux => "mux",
        RuntimeTerminalHostAgentFactoryRouting::TerminalHost => "host",
    }
}

fn terminal_host_resize_label(host: &RuntimeTerminalHostStatus) -> &'static str {
    match host
        .capabilities
        .as_ref()
        .map(|capabilities| capabilities.absolute_resize)
    {
        Some(true) => "absolute",
        Some(false) => "unsupported",
        None => "unknown",
    }
}

fn terminal_host_diagnostic_severity_label(
    severity: brehon_daemon::RuntimeTerminalHostDiagnosticSeverity,
) -> &'static str {
    match severity {
        brehon_daemon::RuntimeTerminalHostDiagnosticSeverity::Info => "info",
        brehon_daemon::RuntimeTerminalHostDiagnosticSeverity::Warning => "warning",
        brehon_daemon::RuntimeTerminalHostDiagnosticSeverity::Error => "error",
    }
}

fn terminal_host_capabilities_summary(
    capabilities: &brehon_types::TerminalHostCapabilities,
) -> String {
    format!(
        "source={:?} pty={} scrollback={} activity={} resize={} lifecycle={} replay={}",
        capabilities.source,
        capabilities.interactive_pty,
        capabilities.scrollback,
        if capabilities.structured_activity {
            "structured"
        } else {
            "unstructured"
        },
        if capabilities.absolute_resize {
            "absolute"
        } else {
            "unsupported"
        },
        if capabilities.out_of_process_lifecycle {
            "out_of_process"
        } else {
            "in_process"
        },
        capabilities.replay
    )
}

fn terminal_host_pane_count(status: &RuntimeDaemonStatus, kind: RuntimeTerminalHostKind) -> usize {
    status
        .registry
        .panes
        .iter()
        .filter(|pane| {
            pane.state != RuntimePaneState::Dead
                && pane
                    .source
                    .as_ref()
                    .is_some_and(|source| source_matches_terminal_host(kind, source))
        })
        .count()
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

fn terminal_host_attach_command(_host: &RuntimeTerminalHostStatus) -> Option<String> {
    None
}

fn status_heartbeat_age_ms(status: &RuntimeDaemonStatus, now_ms: u64) -> u64 {
    now_ms.saturating_sub(status.generated_at_ms)
}

fn status_is_stale(status: &RuntimeDaemonStatus, now_ms: u64) -> bool {
    status.running && status_heartbeat_age_ms(status, now_ms) > STALE_RUNTIME_STATUS_AFTER_MS
}

fn approval_resolution_status_error(status: &RuntimeDaemonStatus, now_ms: u64) -> Option<String> {
    if !status.running {
        return Some("runtime daemon is stopped".to_string());
    }
    if status_is_stale(status, now_ms) {
        return Some(format!(
            "runtime daemon heartbeat is stale (age={}ms)",
            status_heartbeat_age_ms(status, now_ms)
        ));
    }
    None
}

fn approval_resolution_live_status_error(
    status: &RuntimeDaemonStatus,
    pending: &PendingApprovalEntry,
    now_ms: u64,
) -> Option<String> {
    if let Some(error) = approval_resolution_status_error(status, now_ms) {
        return Some(error);
    }
    if status.generated_at_ms < pending.requested_at_ms {
        return None;
    }
    let Some(live) = status
        .approvals
        .approvals
        .iter()
        .find(|approval| approval.approval_id == pending.approval_id)
    else {
        return Some(format!(
            "approval '{}' is not pending in live daemon status",
            pending.approval_id
        ));
    };
    let live_session = live.command.target.session_id.as_str();
    let store_session = pending.command.target.session_id.as_str();
    if live_session != store_session {
        return Some(format!(
            "approval '{}' targets session '{}' in live daemon status, but the approval store targets session '{}'",
            pending.approval_id, live_session, store_session
        ));
    }
    None
}

fn approval_status_warning_for_status(status: &RuntimeDaemonStatus, now_ms: u64) -> Option<String> {
    approval_resolution_status_error(status, now_ms).map(|error| {
        format!("warning: {error}; approve/deny commands are disabled until the daemon is live")
    })
}

async fn approval_status_warning(project_path: Option<&Path>) -> Option<String> {
    let path = daemon_status_path(project_path);
    match read_status(&path).await {
        Ok(status) => approval_status_warning_for_status(&status, unix_timestamp_ms()),
        Err(_) => Some(format!(
            "warning: daemon status unavailable at {}; approve/deny commands may be stale",
            path.display()
        )),
    }
}

pub async fn approvals(project_path: Option<&Path>, json: bool) -> Result<()> {
    let store = read_approval_store(&approval_store_path(project_path)).await?;
    if json {
        let store = store.unwrap_or_else(empty_approval_store);
        println!("{}", serde_json::to_string_pretty(&store)?);
        return Ok(());
    }

    let Some(store) = store else {
        println!("no pending approvals");
        return Ok(());
    };
    if store.approvals.is_empty() {
        println!("no pending approvals");
        return Ok(());
    }

    if let Some(warning) = approval_status_warning(project_path).await {
        println!("{warning}");
    }
    println!(
        "pending approvals{}:",
        store
            .session_id
            .as_ref()
            .map(|session| format!(" for {session}"))
            .unwrap_or_default()
    );
    for approval in store.approvals {
        println!(
            "{}  target={} pane={} command={:?}",
            approval.approval_id,
            approval.command.target.session_id,
            approval
                .command
                .target
                .pane_id
                .as_deref()
                .unwrap_or("runtime"),
            approval.command.kind
        );
        println!("  reason: {}", approval.reason);
    }
    Ok(())
}

pub async fn resolve_approval(
    project_path: Option<&Path>,
    approval_id: &str,
    approved: bool,
    wait_ms: u64,
) -> Result<()> {
    let store_path = approval_store_path(project_path);
    let store = read_approval_store(&store_path)
        .await?
        .with_context(|| format!("no approval store found at {}", store_path.display()))?;
    let pending = store
        .approvals
        .iter()
        .find(|approval| approval.approval_id == approval_id)
        .with_context(|| format!("approval '{approval_id}' was not found"))?;
    let session_id = store
        .session_id
        .clone()
        .unwrap_or_else(|| pending.command.target.session_id.clone());
    let status_path = daemon_status_path(project_path);
    let status = read_status(&status_path).await.with_context(|| {
        format!(
            "cannot resolve runtime approval without daemon status at {}",
            status_path.display()
        )
    })?;
    if let Some(error) =
        approval_resolution_live_status_error(&status, pending, unix_timestamp_ms())
    {
        bail!("cannot resolve runtime approval: {error}");
    }

    let request_id = format!("runtime-cli-{}", uuid::Uuid::new_v4());
    let command_id = format!("resolve-{request_id}");
    let request = RuntimeCommandInboxRequest {
        request_id: request_id.clone(),
        created_at_ms: unix_timestamp_ms(),
        command: RuntimeCommand {
            command_id,
            target: RuntimeCommandTarget {
                session_id,
                pane_id: None,
                generation: None,
            },
            issued_at_ms: unix_timestamp_ms(),
            kind: RuntimeCommandKind::ResolveApproval {
                approval_id: approval_id.to_string(),
                approved,
            },
        },
        context: RuntimePolicyContext::default(),
    };

    let pending_path = command_pending_path(project_path, &request_id);
    let encoded = serde_json::to_vec_pretty(&request)?;
    write_atomic(&pending_path, encoded).await?;

    if wait_ms == 0 {
        println!("queued approval decision {request_id}");
        return Ok(());
    }

    let result_path = command_result_path(project_path, &request_id);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
    while tokio::time::Instant::now() < deadline {
        if let Some(result) = read_inbox_result(&result_path).await? {
            if let Some(error) = result.error {
                bail!("approval decision failed: {error}");
            }
            let Some(command_result) = result.result else {
                bail!("approval decision completed without a command result");
            };
            println!("approval decision: {:?}", command_result.status);
            if let Some(message) = command_result.message {
                println!("{message}");
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!("queued approval decision {request_id}; no daemon result after {wait_ms}ms");
    Ok(())
}

async fn read_status(path: &Path) -> Result<RuntimeDaemonStatus> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read runtime daemon status at {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| {
        format!(
            "failed to decode runtime daemon status at {}",
            path.display()
        )
    })
}

async fn read_approval_store(path: &Path) -> Result<Option<ApprovalStoreSnapshot>> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read approval store at {}", path.display()));
        }
    };
    serde_json::from_str(&contents)
        .map(Some)
        .with_context(|| format!("failed to decode approval store at {}", path.display()))
}

async fn read_inbox_result(path: &Path) -> Result<Option<RuntimeCommandInboxResult>> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read command result at {}", path.display()));
        }
    };
    serde_json::from_str(&contents)
        .map(Some)
        .with_context(|| format!("failed to decode command result at {}", path.display()))
}

fn daemon_status_path(project_path: Option<&Path>) -> PathBuf {
    daemon_dir(project_path).join("current.json")
}

fn approval_store_path(project_path: Option<&Path>) -> PathBuf {
    daemon_dir(project_path).join("approvals.json")
}

fn command_pending_path(project_path: Option<&Path>, request_id: &str) -> PathBuf {
    command_root(project_path)
        .join("pending")
        .join(format!("{request_id}.json"))
}

fn command_result_path(project_path: Option<&Path>, request_id: &str) -> PathBuf {
    command_root(project_path)
        .join("results")
        .join(format!("{request_id}.json"))
}

fn command_root(project_path: Option<&Path>) -> PathBuf {
    daemon_dir(project_path).join("commands")
}

fn daemon_dir(project_path: Option<&Path>) -> PathBuf {
    brehon_root(project_path).join("runtime").join("daemon")
}

fn brehon_root(project_path: Option<&Path>) -> PathBuf {
    project_path
        .map(|path| path.join(".brehon"))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(".brehon"))
}

fn empty_approval_store() -> ApprovalStoreSnapshot {
    ApprovalStoreSnapshot {
        schema_version: 1,
        session_id: None,
        written_at_ms: unix_timestamp_ms(),
        approvals: Vec::new(),
    }
}

async fn write_atomic(path: &Path, encoded: Vec<u8>) -> Result<()> {
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

    fn runtime_status_fixture() -> RuntimeDaemonStatus {
        RuntimeDaemonStatus {
            generated_at_ms: 2,
            started_at_ms: 1,
            uptime_ms: 1,
            running: true,
            audit_log_path: Some("/tmp/brehon-audit.jsonl".to_string()),
            approval_store_path: None,
            metrics: brehon_daemon::RuntimeDaemonMetrics {
                event_capacity: 128,
                subscriber_count: 1,
                published_events: 3,
                routed_commands: 2,
                rejected_commands: 1,
                deferred_commands: 0,
                approval_required_commands: 0,
                pending_approvals: 0,
                audit_write_errors: 0,
            },
            registry_count: 1,
            registry: brehon_daemon::PaneRegistrySnapshot {
                generated_at_ms: 2,
                panes: vec![brehon_daemon::PaneRegistryEntry {
                    session_id: "session".to_string(),
                    pane_id: "pane".to_string(),
                    generation: 7,
                    state: brehon_types::RuntimePaneState::Dead,
                    kind: brehon_types::RuntimePaneKind::Worker,
                    source: Some(brehon_types::RuntimeSource::Headless),
                    title: Some("worker".to_string()),
                    last_event_ms: 2,
                    last_output_ms: Some(10),
                    exit_code: Some(1),
                    exit_reason: Some("done".to_string()),
                }],
            },
            approvals: brehon_daemon::ApprovalRegistrySnapshot {
                generated_at_ms: 2,
                approvals: Vec::new(),
            },
            sidecar: Some(brehon_daemon::RuntimeSidecarStatus {
                detection_running: true,
                workflow_running: false,
            }),
            terminal_host: Some(brehon_daemon::RuntimeTerminalHostStatus {
                kind: brehon_types::RuntimeTerminalHostKind::Headless,
                experimental: true,
                observation_running: true,
                command_routing: brehon_daemon::RuntimeTerminalHostCommandRouting::TerminalHost,
                pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Host,
                agent_factory: RuntimeTerminalHostAgentFactoryRouting::Mux,
                capabilities: Some(brehon_types::TerminalHostCapabilities {
                    source: RuntimeSource::Headless,
                    interactive_pty: true,
                    scrollback: true,
                    structured_activity: true,
                    absolute_resize: false,
                    out_of_process_lifecycle: true,
                    replay: false,
                }),
                promotion_readiness: brehon_daemon::RuntimeTerminalHostPromotionReadiness::default(),
                session_name: Some("brehon-session".to_string()),
                socket_name: None,
                socket_dir: None,
                binary_path: None,
                diagnostics: vec![brehon_daemon::RuntimeTerminalHostDiagnostic {
                    severity: brehon_daemon::RuntimeTerminalHostDiagnosticSeverity::Warning,
                    code: "terminal_host_absolute_resize_unsupported".to_string(),
                    message: "Headless does not advertise absolute resize".to_string(),
                    action: Some(
                        "keep promotion blocked or use a host that supports absolute pane resize"
                            .to_string(),
                    ),
                }],
            }),
        }
    }

    fn pending_approval_fixture(requested_at_ms: u64) -> PendingApprovalEntry {
        PendingApprovalEntry {
            approval_id: "approval-1".to_string(),
            requested_at_ms,
            reason: "operation requires approval".to_string(),
            command: RuntimeCommand {
                command_id: "cmd-1".to_string(),
                target: RuntimeCommandTarget {
                    session_id: "session".to_string(),
                    pane_id: Some("pane".to_string()),
                    generation: Some(1),
                },
                issued_at_ms: requested_at_ms,
                kind: RuntimeCommandKind::Interrupt {
                    reason: "operator".to_string(),
                },
            },
            context: RuntimePolicyContext::default(),
        }
    }

    #[test]
    fn status_text_includes_registry_pane_details() {
        let status = runtime_status_fixture();
        let text = format_status_text_at(&status, 12);

        assert!(text.contains("runtime daemon: running"));
        assert!(text.contains("heartbeat_age_ms: 10"));
        assert!(text.contains(
            "terminal_host: kind=Headless mode=host-owned experimental=true observation=true commands=TerminalHost pane_owner=host agent_factory=mux resize=unsupported host_panes=0"
        ));
        assert!(text.contains("terminal_host_session: brehon-session"));
        assert!(text.contains(
            "terminal_host_capabilities: source=Headless pty=true scrollback=true activity=structured resize=unsupported lifecycle=out_of_process replay=false"
        ));
        assert!(text.contains("terminal_host_promotion: blocked blockers=2"));
        assert!(text.contains(
            "terminal_host_promotion_blocker: worker/reviewer/supervisor factory still mux-owned"
        ));
        assert!(text.contains(
            "terminal_host_promotion_blocker: terminal host does not advertise absolute resize"
        ));
        assert!(text.contains(
            "terminal_host_diagnostic: severity=warning code=terminal_host_absolute_resize_unsupported message=Headless does not advertise absolute resize"
        ));
        assert!(
            text.contains("terminal_host_diagnostic_action: keep promotion blocked or use a host that supports absolute pane resize")
        );
        assert!(text.contains("sidecar: detection=true workflow=false"));
        assert!(text.contains("pane_sources: Headless=1"));
        assert!(text.contains("registry:"));
        assert!(text.contains("session/pane gen=7 state=Dead kind=Worker"));
        assert!(text.contains("source=Headless"));
        assert!(text.contains("title=\"worker\""));
        assert!(text.contains("last_output_ms=10"));
        assert!(text.contains("exit_code=1"));
        assert!(text.contains("exit_reason=\"done\""));
    }

    #[test]
    fn status_text_marks_external_host_preview_mode_when_commands_remain_mux_owned() {
        let mut status = runtime_status_fixture();
        status.registry.panes[0].state = RuntimePaneState::Ready;
        status.registry.panes[0].exit_code = None;
        status.registry.panes[0].exit_reason = None;
        let host = status.terminal_host.as_mut().expect("terminal host");
        host.command_routing = RuntimeTerminalHostCommandRouting::Mux;
        host.pane_ownership = RuntimeTerminalHostPaneOwnership::Mux;

        let text = format_status_text_at(&status, 12);

        assert!(text.contains(
            "terminal_host: kind=Headless mode=preview experimental=true observation=true commands=Mux pane_owner=mux agent_factory=mux resize=unsupported host_panes=1"
        ));
    }

    #[test]
    fn status_text_ignores_dead_host_panes_for_preview_mode() {
        let mut status = runtime_status_fixture();
        let host = status.terminal_host.as_mut().expect("terminal host");
        host.command_routing = RuntimeTerminalHostCommandRouting::Mux;
        host.pane_ownership = RuntimeTerminalHostPaneOwnership::Mux;

        let text = format_status_text_at(&status, 12);

        assert!(text.contains(
            "terminal_host: kind=Headless mode=standby experimental=true observation=true commands=Mux pane_owner=mux agent_factory=mux resize=unsupported host_panes=0"
        ));
    }

    #[test]
    fn status_text_marks_running_daemon_stale_when_heartbeat_is_old() {
        let status = runtime_status_fixture();
        let now_ms = status
            .generated_at_ms
            .saturating_add(STALE_RUNTIME_STATUS_AFTER_MS + 1);
        let text = format_status_text_at(&status, now_ms);

        assert!(text.contains("runtime daemon: stale"));
        assert!(text.contains("reported_running: true"));
        assert!(text.contains("heartbeat_age_ms: 15001"));
    }

    #[test]
    fn approval_resolution_requires_live_daemon_status() {
        let mut status = runtime_status_fixture();
        assert_eq!(approval_resolution_status_error(&status, 12), None);
        assert_eq!(approval_status_warning_for_status(&status, 12), None);

        status.running = false;
        assert_eq!(
            approval_resolution_status_error(&status, 12).as_deref(),
            Some("runtime daemon is stopped")
        );
        assert_eq!(
            approval_status_warning_for_status(&status, 12).as_deref(),
            Some(
                "warning: runtime daemon is stopped; approve/deny commands are disabled until the daemon is live"
            )
        );

        status.running = true;
        let now_ms = status
            .generated_at_ms
            .saturating_add(STALE_RUNTIME_STATUS_AFTER_MS + 1);
        assert_eq!(
            approval_resolution_status_error(&status, now_ms).as_deref(),
            Some("runtime daemon heartbeat is stale (age=15001ms)")
        );
        assert_eq!(
            approval_status_warning_for_status(&status, now_ms).as_deref(),
            Some(
                "warning: runtime daemon heartbeat is stale (age=15001ms); approve/deny commands are disabled until the daemon is live"
            )
        );
    }

    #[test]
    fn approval_resolution_cross_checks_live_status_when_snapshot_is_current() {
        let pending = pending_approval_fixture(10);
        let mut status = runtime_status_fixture();
        status.generated_at_ms = 20;
        status.approvals.generated_at_ms = 20;

        assert_eq!(
            approval_resolution_live_status_error(&status, &pending, 21).as_deref(),
            Some("approval 'approval-1' is not pending in live daemon status")
        );

        status.approvals.approvals.push(pending.clone());
        assert_eq!(
            approval_resolution_live_status_error(&status, &pending, 21),
            None
        );

        status.approvals.approvals[0].command.target.session_id = "other-session".to_string();
        assert_eq!(
            approval_resolution_live_status_error(&status, &pending, 21).as_deref(),
            Some(
                "approval 'approval-1' targets session 'other-session' in live daemon status, but the approval store targets session 'session'"
            )
        );

        let mut older_status = runtime_status_fixture();
        older_status.generated_at_ms = 5;
        assert_eq!(
            approval_resolution_live_status_error(&older_status, &pending, 6),
            None
        );
    }
}
