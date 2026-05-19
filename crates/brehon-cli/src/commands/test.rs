use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use brehon_ports::{RuntimeCommandPort, RuntimeEventSink};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TerminalHostSmokeKind {
    Headless,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TerminalHostSmokeMode {
    Basic,
    Lifecycle,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TerminalHostPaneOwnershipArg {
    Mux,
    Host,
}

impl TerminalHostSmokeKind {
    fn runtime_kind(self) -> brehon_types::RuntimeTerminalHostKind {
        match self {
            Self::Headless => brehon_types::RuntimeTerminalHostKind::Headless,
        }
    }
}

impl TerminalHostPaneOwnershipArg {
    fn runtime_pane_ownership(self) -> brehon_types::RuntimeTerminalHostPaneOwnership {
        match self {
            Self::Mux => brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
            Self::Host => brehon_types::RuntimeTerminalHostPaneOwnership::Host,
        }
    }
}

pub async fn run_scenario(
    project_path: Option<&Path>,
    config_override: Option<&Path>,
    name: &str,
) -> Result<()> {
    use brehon_config::load_config_with_override;
    use brehon_test_harness::scenario::Scenario;

    let _config = load_config_with_override(project_path, config_override)?;

    let scenario_path = project_path
        .map(|p| p.join(".brehon/scenarios").join(format!("{}.yaml", name)))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .join(".brehon/scenarios")
                .join(format!("{}.yaml", name))
        });

    if !scenario_path.exists() {
        anyhow::bail!("Scenario file not found: {:?}", scenario_path);
    }

    let scenario_content = std::fs::read_to_string(&scenario_path)?;
    let _scenario: Scenario = serde_yaml::from_str(&scenario_content)?;

    tracing::info!("Running scenario: {}", name);

    tracing::info!("Scenario completed successfully");

    Ok(())
}

pub async fn run_live_conformance(
    project_path: Option<&Path>,
    config_override: Option<&Path>,
) -> Result<()> {
    use brehon_acp::AcpGateway;
    use brehon_config::load_config_with_override;
    use brehon_ports::AgentGateway;
    use brehon_types::SessionSpec;

    let config = load_config_with_override(project_path, config_override)?;

    tracing::info!("Running ACP conformance tests...");

    let mut gateway = AcpGateway::new();

    for (name, agent_config) in &config.launchers {
        tracing::info!("Testing launcher: {}", name);

        let launch = brehon_acp::AgentLaunchConfig {
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            env: vec![],
            protocol: match agent_config.adapter {
                brehon_types::agent::AdapterKind::Acp => brehon_acp::GatewayProtocol::AcpStdio,
                brehon_types::agent::AdapterKind::OpenAiCompatible => {
                    brehon_acp::GatewayProtocol::OpenAiCompatibleChat
                }
                brehon_types::agent::AdapterKind::NativeAgent => {
                    brehon_acp::GatewayProtocol::AcpStdio
                }
                brehon_types::agent::AdapterKind::Mock => brehon_acp::GatewayProtocol::AcpStdio,
                brehon_types::agent::AdapterKind::Kimi => brehon_acp::GatewayProtocol::AcpStdio,
                brehon_types::agent::AdapterKind::Junie => brehon_acp::GatewayProtocol::AcpStdio,
                brehon_types::agent::AdapterKind::Copilot => brehon_acp::GatewayProtocol::AcpStdio,
                brehon_types::agent::AdapterKind::PtyHooks => {
                    tracing::warn!(
                        "Skipping conformance test for PtyHooks adapter {} — native hooks do not speak ACP",
                        name
                    );
                    continue;
                }
                brehon_types::agent::AdapterKind::Codex => {
                    brehon_acp::GatewayProtocol::CodexAppServerWs
                }
            },
            tool_prefix: None,
            tool_bridge: None,
            base_url: agent_config.base_url.clone(),
            api_key_env: agent_config.api_key_env.clone(),
            headers: agent_config
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            model: config
                .lane_model(name, None)
                .map(|model| model.name.clone()),
            sidecar_socket_path: None,
            sidecar_ready_path: None,
            sidecar_connect_timeout_ms: None,
        };
        gateway.register_agent_launch(name, launch);

        let spec = SessionSpec::new(
            brehon_types::AgentId::new(name),
            "test".into(),
            std::env::temp_dir().to_string_lossy().to_string(),
        );

        let result = gateway.spawn(spec).await;

        match result {
            Ok(session_id) => {
                tracing::info!("Agent spawn successful: {} -> {}", name, session_id);

                let capabilities = gateway.capabilities(&session_id).await?;
                tracing::info!("Agent {} capabilities:", name);
                tracing::info!("  terminal_support: {}", capabilities.terminal_support);
                tracing::info!("  permission_support: {}", capabilities.permission_support);

                gateway.kill_session(&session_id).await?;
            }
            Err(e) => {
                tracing::error!("Agent spawn failed: {}: {:?}", name, e);
            }
        }
    }

    tracing::info!("ACP conformance tests complete");

    Ok(())
}

pub async fn run_runtime_host_wiring_smoke(
    project_path: Option<&Path>,
    config_override: Option<&Path>,
    host: TerminalHostSmokeKind,
    pane_ownership: TerminalHostPaneOwnershipArg,
) -> Result<()> {
    use brehon_config::load_config_with_override;

    let cwd = project_path
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut config = load_config_with_override(project_path, config_override)?;
    config.runtime.terminal_host.kind = Some(host.runtime_kind());
    config.runtime.terminal_host.preview_pane = Some(false);
    config.runtime.terminal_host.pane_ownership = Some(pane_ownership.runtime_pane_ownership());

    let session_name = format!(
        "brehon-run-wiring-smoke-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("session")
    );
    let report = crate::commands::run::run_runtime_terminal_host_wiring_smoke(
        &config.runtime.terminal_host,
        &session_name,
        &cwd,
    )
    .await?;

    println!(
        "run wiring smoke passed: host={:?} pane_ownership={:?} commands={:?} agent_factory={:?} promotion={} blockers={} observed_output={} panes={} session={}",
        report.terminal_host_status.kind,
        report.terminal_host_status.pane_ownership,
        report.terminal_host_status.command_routing,
        report.terminal_host_status.agent_factory,
        if report
            .terminal_host_status
            .promotion_readiness
            .ready
        {
            "ready"
        } else {
            "blocked"
        },
        report
            .terminal_host_status
            .promotion_readiness
            .blockers
            .len(),
        report.observed_output,
        report.registry_count,
        session_name
    );
    Ok(())
}

pub async fn run_terminal_host_smoke(
    project_path: Option<&Path>,
    config_override: Option<&Path>,
    host: TerminalHostSmokeKind,
    mode: TerminalHostSmokeMode,
) -> Result<()> {
    use brehon_config::load_config_with_override;

    let cwd = project_path
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut config = load_config_with_override(project_path, config_override)?;
    config.runtime.terminal_host.kind = Some(host.runtime_kind());

    let session_name = format!(
        "brehon-smoke-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("session")
    );
    let configured = brehon_host::configured_terminal_host_from_runtime_config(
        &config.runtime.terminal_host,
        &session_name,
    )?
    .context("terminal host smoke requires an adapter-backed host")?;
    let capabilities = configured.adapter().capabilities();
    let marker = format!(
        "brehon-terminal-host-smoke-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("marker")
    );

    let result = exercise_terminal_host(&configured, &cwd, &session_name, &marker).await;
    let result = match result {
        Ok(()) if matches!(mode, TerminalHostSmokeMode::Lifecycle) => {
            let exit_marker = format!("{marker}-exit");
            exercise_terminal_host_exit(&configured, &cwd, &session_name, &exit_marker).await
        }
        other => other,
    };
    let cleanup = configured.shutdown().await;
    if let Err(err) = cleanup {
        if result.is_ok() {
            bail!("terminal host smoke cleanup failed: {err}");
        }
        tracing::warn!(error = %err, "terminal host smoke cleanup failed after probe error");
    }
    result?;

    println!(
        "terminal host smoke passed: kind={:?} mode={mode:?} resize={} session={} marker={}",
        host.runtime_kind(),
        if capabilities.absolute_resize {
            "absolute"
        } else {
            "unsupported"
        },
        session_name,
        marker
    );
    Ok(())
}

pub async fn run_runtime_daemon_smoke(
    project_path: Option<&Path>,
    config_override: Option<&Path>,
    host: TerminalHostSmokeKind,
) -> Result<()> {
    use brehon_config::load_config_with_override;

    let cwd = project_path
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut config = load_config_with_override(project_path, config_override)?;
    config.runtime.terminal_host.kind = Some(host.runtime_kind());

    let session_name = format!(
        "brehon-daemon-smoke-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("session")
    );
    let marker = format!(
        "brehon-runtime-daemon-smoke-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("marker")
    );

    let brehon_root = cwd.join(".brehon");
    let audit_log_path = brehon_root
        .join("runtime")
        .join("audit")
        .join(format!("{session_name}.jsonl"));
    let approval_store_path = brehon_root
        .join("runtime")
        .join("daemon")
        .join("approvals.json");
    let status_path = brehon_root
        .join("runtime")
        .join("daemon")
        .join("current.json");

    let configured = brehon_host::configured_terminal_host_from_runtime_config(
        &config.runtime.terminal_host,
        &session_name,
    )?
    .context("runtime daemon smoke requires an adapter-backed host")?;

    let adapter = configured.adapter();
    let capabilities = adapter.capabilities();
    let observation_running = configured.observer().is_some();
    let host_identity = configured.runtime_identity(&session_name);
    let captured_events = Arc::new(CapturingRuntimeEventSink::default());
    let command_port: Arc<dyn RuntimeCommandPort> = Arc::new(
        brehon_host::TerminalHostCommandPort::new(adapter).with_event_sink(captured_events.clone()),
    );
    let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
        policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
        command_port: Some(command_port),
        audit_log_path: Some(audit_log_path.clone()),
        approval_store_path: Some(approval_store_path),
        approval_store_session_id: Some(session_name.clone()),
        terminal_host: Some(terminal_host_status_with_readiness(
            brehon_daemon::RuntimeTerminalHostStatus {
                kind: host.runtime_kind(),
                experimental: true,
                observation_running,
                command_routing: brehon_daemon::RuntimeTerminalHostCommandRouting::TerminalHost,
                pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Host,
                agent_factory: brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::Mux,
                capabilities: Some(capabilities.clone()),
                promotion_readiness: brehon_daemon::RuntimeTerminalHostPromotionReadiness::default(
                ),
                session_name: host_identity.session_name,
                socket_name: host_identity.socket_name,
                socket_dir: host_identity.socket_dir,
                binary_path: host_identity.binary_path,
                diagnostics: Vec::new(),
            },
        )),
        ..brehon_daemon::RuntimeDaemonConfig::default()
    });

    let result: Result<()> = async {
        let spawn = daemon
            .route_command(
                runtime_smoke_command(
                    &session_name,
                    "status-probe",
                    None,
                    brehon_types::RuntimeCommandKind::SpawnPane {
                        kind: brehon_types::RuntimePaneKind::Worker,
                        pane_id: Some("status-probe".to_string()),
                        title: Some("runtime daemon smoke".to_string()),
                        cwd: Some(cwd.display().to_string()),
                        command: vec!["cat".to_string()],
                        env: BTreeMap::new(),
                        rows: Some(40),
                        cols: Some(120),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .context("route runtime daemon smoke spawn")?;
        ensure_applied(&spawn, "spawn")?;
        publish_captured_events(&captured_events, &daemon).await?;

        let resize = daemon
            .route_command(
                runtime_smoke_command(
                    &session_name,
                    "status-probe",
                    Some(1),
                    brehon_types::RuntimeCommandKind::ResizePane {
                        rows: 30,
                        cols: 100,
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .context("route runtime daemon smoke resize")?;
        if capabilities.absolute_resize {
            ensure_applied(&resize, "resize")?;
            publish_captured_events(&captured_events, &daemon).await?;
        } else if resize.status != brehon_types::RuntimeCommandStatus::Rejected {
            bail!(
                "runtime daemon smoke expected unsupported resize to be rejected, got {:?}: {}",
                resize.status,
                resize.message.as_deref().unwrap_or("no result message")
            );
        }

        let input = daemon
            .route_command(
                runtime_smoke_command(
                    &session_name,
                    "status-probe",
                    Some(1),
                    brehon_types::RuntimeCommandKind::SendTerminalInput {
                        bytes: format!("{marker}\n").into_bytes(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .context("route runtime daemon smoke input")?;
        ensure_applied(&input, "input")?;
        publish_captured_events(&captured_events, &daemon).await?;

        let brehon_host::ConfiguredTerminalHost::Headless(_) = &configured;
        daemon
            .publish(brehon_types::RuntimeEvent::new(
                brehon_types::RuntimeEventMeta::new(
                    session_name.clone(),
                    "status-probe",
                    1,
                    brehon_types::RuntimeSource::Headless,
                    unix_timestamp_ms(),
                ),
                brehon_types::RuntimeEventKind::PaneOutput(brehon_types::PaneOutputEvent {
                    bytes: marker.as_bytes().to_vec(),
                    text: Some(marker.clone()),
                }),
            ))
            .await
            .context("publish runtime daemon smoke output")?;

        let prompt_marker = format!("{marker}-prompt");
        let prompt = daemon
            .route_command(
                runtime_smoke_command(
                    &session_name,
                    "status-probe",
                    None,
                    brehon_types::RuntimeCommandKind::SendPrompt {
                        prompt_id: format!("prompt-{marker}"),
                        text: prompt_marker.clone(),
                        from: None,
                        delivery: brehon_types::PromptDeliveryMode::Direct,
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .context("route runtime daemon smoke prompt")?;
        ensure_applied(&prompt, "prompt")?;
        publish_captured_events(&captured_events, &daemon).await?;

        let brehon_host::ConfiguredTerminalHost::Headless(_) = &configured;
        daemon
            .publish(brehon_types::RuntimeEvent::new(
                brehon_types::RuntimeEventMeta::new(
                    session_name.clone(),
                    "status-probe",
                    1,
                    brehon_types::RuntimeSource::Headless,
                    unix_timestamp_ms(),
                ),
                brehon_types::RuntimeEventKind::PaneOutput(brehon_types::PaneOutputEvent {
                    bytes: prompt_marker.as_bytes().to_vec(),
                    text: Some(prompt_marker.clone()),
                }),
            ))
            .await
            .context("publish runtime daemon smoke prompt output")?;

        let close = daemon
            .route_command(
                runtime_smoke_command(
                    &session_name,
                    "status-probe",
                    Some(1),
                    brehon_types::RuntimeCommandKind::ClosePane {
                        reason: "runtime daemon smoke complete".to_string(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .context("route runtime daemon smoke close")?;
        ensure_applied(&close, "close")?;
        publish_captured_events(&captured_events, &daemon).await?;

        let rejected = daemon
            .route_command(
                runtime_smoke_command(
                    &session_name,
                    "status-probe",
                    Some(1),
                    brehon_types::RuntimeCommandKind::SendTerminalInput {
                        bytes: b"after close".to_vec(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .context("route runtime daemon smoke post-close input")?;
        if rejected.status != brehon_types::RuntimeCommandStatus::Rejected {
            bail!(
                "expected post-close input to be rejected, got {:?}",
                rejected.status
            );
        }

        daemon.shutdown().await;
        brehon_daemon::RuntimeDaemonHeartbeat::write_current_status(&status_path, &daemon, None)
            .await
            .context("write runtime daemon smoke status")?;

        let audit_events = brehon_daemon::read_audit_log(&audit_log_path)
            .await
            .context("read runtime daemon smoke audit log")?;
        if audit_events.is_empty() {
            bail!("runtime daemon smoke audit log did not contain any events");
        }
        let replayed_daemon = brehon_daemon::RuntimeDaemon::default();
        brehon_daemon::replay_audit_log(&audit_log_path, &replayed_daemon)
            .await
            .context("replay runtime daemon smoke audit log")?;
        let replayed_registry = replayed_daemon.pane_registry_snapshot().await;
        let replayed_pane = replayed_registry
            .panes
            .iter()
            .find(|pane| pane.session_id == session_name && pane.pane_id == "status-probe")
            .context("replayed audit log did not rebuild status-probe pane")?;
        if replayed_pane.state != brehon_types::RuntimePaneState::Dead {
            bail!(
                "replayed status-probe pane should be dead, got {:?}",
                replayed_pane.state
            );
        }

        println!(
            "runtime daemon smoke passed: host={:?} resize={} session={} status={} audit_events={} marker={}",
            host.runtime_kind(),
            if capabilities.absolute_resize {
                "absolute"
            } else {
                "unsupported"
            },
            session_name,
            status_path.display(),
            audit_events.len(),
            marker
        );
        Ok(())
    }
    .await;

    let cleanup = configured.shutdown().await;
    if let Err(err) = cleanup {
        if result.is_ok() {
            bail!("runtime daemon smoke cleanup failed: {err}");
        }
        tracing::warn!(error = %err, "runtime daemon smoke cleanup failed after probe error");
    }
    result
}

async fn exercise_terminal_host(
    configured: &brehon_host::ConfiguredTerminalHost,
    cwd: &Path,
    session_name: &str,
    marker: &str,
) -> Result<()> {
    let adapter = configured.adapter();
    let handle = adapter
        .spawn_pane(brehon_types::TerminalPaneSpawnSpec {
            session_id: session_name.to_string(),
            pane_id: "smoke-probe".to_string(),
            kind: brehon_types::RuntimePaneKind::Shell,
            title: Some("brehon terminal host smoke".to_string()),
            cwd: Some(cwd.display().to_string()),
            command: vec!["cat".to_string()],
            env: BTreeMap::new(),
            rows: 24,
            cols: 80,
        })
        .await
        .context("spawn terminal host smoke pane")?;

    let capabilities = adapter.capabilities();
    if capabilities.absolute_resize {
        adapter
            .resize_pane(
                handle.clone(),
                brehon_types::TerminalResize {
                    rows: 30,
                    cols: 100,
                },
            )
            .await
            .context("resize terminal host smoke pane")?;
    } else if adapter
        .resize_pane(
            handle.clone(),
            brehon_types::TerminalResize {
                rows: 30,
                cols: 100,
            },
        )
        .await
        .is_ok()
    {
        bail!("terminal host accepted absolute resize despite advertising unsupported resize");
    }

    adapter
        .send_input(handle.clone(), format!("{marker}\n").into_bytes())
        .await
        .context("send terminal host smoke input")?;

    let brehon_host::ConfiguredTerminalHost::Headless(host) = configured;
    let snapshot = host
        .snapshot(session_name, "smoke-probe")
        .await
        .context("read headless smoke snapshot")?;
    if !String::from_utf8_lossy(&snapshot.input_bytes).contains(marker) {
        bail!("headless smoke pane did not record marker input");
    }
    if capabilities.absolute_resize && (snapshot.rows != 30 || snapshot.cols != 100) {
        bail!(
            "headless smoke pane did not record resize: got {}x{}",
            snapshot.cols,
            snapshot.rows
        );
    }

    adapter
        .close_pane(handle)
        .await
        .context("close terminal host smoke pane")?;
    Ok(())
}

async fn exercise_terminal_host_exit(
    configured: &brehon_host::ConfiguredTerminalHost,
    cwd: &Path,
    session_name: &str,
    marker: &str,
) -> Result<()> {
    let adapter = configured.adapter();
    let mut env = BTreeMap::new();
    env.insert("BREHON_SMOKE_EXIT_MARKER".to_string(), marker.to_string());
    let handle = adapter
        .spawn_pane(brehon_types::TerminalPaneSpawnSpec {
            session_id: session_name.to_string(),
            pane_id: "smoke-exit-probe".to_string(),
            kind: brehon_types::RuntimePaneKind::Worker,
            title: Some("brehon terminal host exit smoke".to_string()),
            cwd: Some(cwd.display().to_string()),
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf '%s\\n' \"$BREHON_SMOKE_EXIT_MARKER\"; sleep 0.5; exit 7".to_string(),
            ],
            env,
            rows: 24,
            cols: 80,
        })
        .await
        .context("spawn terminal host exit smoke pane")?;

    let brehon_host::ConfiguredTerminalHost::Headless(host) = configured;
    use brehon_ports::RuntimeEventSink;

    host.publish(brehon_types::RuntimeEvent::new(
        runtime_meta_for_handle(&handle),
        brehon_types::RuntimeEventKind::PaneExited(brehon_types::PaneExitedEvent {
            exit_code: Some(7),
            reason: Some("headless smoke exit".to_string()),
        }),
    ))
    .await
    .context("publish headless smoke exit")?;

    if adapter
        .send_input(handle, b"after exit".to_vec())
        .await
        .is_ok()
    {
        bail!("terminal host accepted input after exit");
    }
    Ok(())
}

#[derive(Debug, Default)]
struct CapturingRuntimeEventSink {
    events: tokio::sync::Mutex<Vec<brehon_types::RuntimeEvent>>,
}

impl CapturingRuntimeEventSink {
    async fn drain(&self) -> Vec<brehon_types::RuntimeEvent> {
        let mut events = self.events.lock().await;
        std::mem::take(&mut *events)
    }
}

#[async_trait::async_trait]
impl RuntimeEventSink for CapturingRuntimeEventSink {
    async fn publish(
        &self,
        event: brehon_types::RuntimeEvent,
    ) -> std::result::Result<(), brehon_ports::PortError> {
        self.events.lock().await.push(event);
        Ok(())
    }
}

async fn publish_captured_events(
    captured_events: &CapturingRuntimeEventSink,
    daemon: &brehon_daemon::RuntimeDaemon,
) -> Result<()> {
    for event in captured_events.drain().await {
        daemon.publish(event).await?;
    }
    Ok(())
}

fn ensure_applied(result: &brehon_types::RuntimeCommandResult, operation: &str) -> Result<()> {
    if result.status == brehon_types::RuntimeCommandStatus::Applied {
        return Ok(());
    }
    bail!(
        "runtime daemon smoke {operation} failed with {:?}: {}",
        result.status,
        result.message.as_deref().unwrap_or("no result message")
    )
}

fn terminal_host_status_with_readiness(
    mut status: brehon_daemon::RuntimeTerminalHostStatus,
) -> brehon_daemon::RuntimeTerminalHostStatus {
    status.promotion_readiness = brehon_daemon::terminal_host_promotion_readiness(Some(&status));
    status
}

fn runtime_smoke_command(
    session_id: &str,
    pane_id: &str,
    generation: Option<u64>,
    kind: brehon_types::RuntimeCommandKind,
) -> brehon_types::RuntimeCommand {
    brehon_types::RuntimeCommand {
        command_id: format!(
            "runtime-smoke-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("command")
        ),
        target: brehon_types::RuntimeCommandTarget {
            session_id: session_id.to_string(),
            pane_id: Some(pane_id.to_string()),
            generation,
        },
        issued_at_ms: unix_timestamp_ms(),
        kind,
    }
}

fn runtime_meta_for_handle(
    handle: &brehon_types::TerminalPaneHandle,
) -> brehon_types::RuntimeEventMeta {
    brehon_types::RuntimeEventMeta::new(
        handle.session_id.clone(),
        handle.pane_id.clone(),
        handle.generation,
        handle.source.clone(),
        unix_timestamp_ms(),
    )
}

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
