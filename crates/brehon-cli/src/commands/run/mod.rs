mod direct_tools;
mod review;
mod setup;
pub(crate) use setup::{effective_worktree_root, normalize_project_root};
#[cfg(test)]
mod startup_reconciliation_tests;
mod workers;
use crate::commands::process::{terminate_project_processes, terminate_session_processes};
use brehon_ports::{
    PolicyGate, PortError, RuntimeCommandPort, RuntimeCommandRouter, RuntimeEventSink,
    RuntimeEventStream,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use direct_tools::BrehonDirectToolBridgeFactory;
use review::{
    build_planned_review_panel_seats, build_reviewer_panels, reconcile_review_runtime_for_run,
};
use setup::{
    activate_claude_worktree_hook, activate_protected_branch_guard, agent_to_adapter,
    cleanup_scoped_worktrees, ensure_claude_worktree_hook, ensure_codex_instruction_files,
    ensure_mcp_config, ensure_protected_branch_hooks, ensure_shared_root_on_default_branch,
    prepare_scoped_worktrees_with_progress, reconcile_initiative_hierarchy_for_run,
    reconcile_orphaned_worker_assignments_for_run, restore_shared_root_branch,
};
pub(crate) use setup::{
    protected_branch_hooks_installed, remove_claude_worktree_hook, remove_protected_branch_hooks,
};
use workers::{push_runtime_dashboard_event, resolve_worker_pool_counts};

const IMPLICIT_PANEL_ID: &str = "default-panel";
const EXPERIMENTAL_TERMINAL_HOST_ENV: &str = "BREHON_EXPERIMENTAL_TERMINAL_HOST";
const EXPERIMENTAL_TERMINAL_HOST_PREVIEW_ENV: &str = "BREHON_EXPERIMENTAL_TERMINAL_HOST_PREVIEW";
const EXPERIMENTAL_TERMINAL_HOST_EAGER_GATEWAY_BOOTSTRAP_ENV: &str =
    "BREHON_EXPERIMENTAL_TERMINAL_HOST_EAGER_GATEWAY_BOOTSTRAP";
const TERMINAL_HOST_PREVIEW_PANE_ID: &str = "host-preview";
const TERMINAL_HOST_STARTUP_PROMPT_DELAY_SECS: u64 = 5;
const TERMINAL_HOST_STARTUP_PROMPT_STAGGER_MILLIS: u64 = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalHostPreviewPane {
    pane_id: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalHostStartupPrompt {
    target: String,
    prompt: String,
    delay: std::time::Duration,
}

#[derive(Default)]
struct LateBindingRuntimeEventSink {
    target: Mutex<Option<Arc<dyn RuntimeEventSink>>>,
}

impl LateBindingRuntimeEventSink {
    fn bind(&self, target: Arc<dyn RuntimeEventSink>) {
        *self.target.lock().unwrap_or_else(|err| err.into_inner()) = Some(target);
    }
}

#[async_trait]
impl RuntimeEventSink for LateBindingRuntimeEventSink {
    async fn publish(
        &self,
        event: brehon_types::RuntimeEvent,
    ) -> std::result::Result<(), PortError> {
        let target = self
            .target
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        if let Some(target) = target {
            target.publish(event).await?;
        }
        Ok(())
    }
}

fn resolved_supervisor_model(
    config: &brehon_types::BrehonConfig,
    format_model: impl Fn(&str, &str, &str) -> String,
) -> Option<String> {
    let lane = config.roles.supervisor.name.as_str();
    let model = config
        .lane_model(lane, None)
        .or(config.supervisor.model.as_ref())?;
    let launcher_name = config.lane_launcher_name(lane).unwrap_or(lane);
    Some(format_model(launcher_name, &model.provider, &model.name))
}

fn resolved_supervisor_reasoning_effort(config: &brehon_types::BrehonConfig) -> Option<String> {
    config
        .lane_reasoning_effort(&config.roles.supervisor.name, None)
        .map(str::to_string)
        .or_else(|| config.supervisor.reasoning_effort.clone())
}

fn ensure_runtime_terminal_host_supported(config: &brehon_types::BrehonConfig) -> Result<()> {
    let host = &config.runtime.terminal_host;
    let kind = host.effective_kind();
    let pane_ownership = host.effective_pane_ownership();
    if !runtime_terminal_host_run_supported(
        kind,
        pane_ownership,
        experimental_terminal_host_enabled(),
    ) {
        if pane_ownership == brehon_types::RuntimeTerminalHostPaneOwnership::Host {
            anyhow::bail!(
                "runtime.terminal_host.pane_ownership=host requires {EXPERIMENTAL_TERMINAL_HOST_ENV}=1 and runtime.terminal_host.kind=headless; got {:?}",
                kind,
            );
        }
        anyhow::bail!(
            "runtime.terminal_host.kind {:?} is not supported in brehon run; use embedded for production or set {EXPERIMENTAL_TERMINAL_HOST_ENV}=1 with kind=headless for daemon/runtime tests",
            kind,
        );
    }
    Ok(())
}

fn runtime_terminal_host_run_supported(
    kind: brehon_types::RuntimeTerminalHostKind,
    pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership,
    experimental_enabled: bool,
) -> bool {
    match pane_ownership {
        brehon_types::RuntimeTerminalHostPaneOwnership::Mux => {
            runtime_terminal_host_supported(kind, experimental_enabled)
        }
        brehon_types::RuntimeTerminalHostPaneOwnership::Host => {
            experimental_enabled && matches!(kind, brehon_types::RuntimeTerminalHostKind::Headless)
        }
    }
}

fn runtime_terminal_host_supported(
    kind: brehon_types::RuntimeTerminalHostKind,
    experimental_enabled: bool,
) -> bool {
    kind == brehon_types::RuntimeTerminalHostKind::Embedded
        || (experimental_enabled && matches!(kind, brehon_types::RuntimeTerminalHostKind::Headless))
}

fn experimental_terminal_host_enabled() -> bool {
    experimental_terminal_host_enabled_from_value(
        std::env::var(EXPERIMENTAL_TERMINAL_HOST_ENV)
            .ok()
            .as_deref(),
    )
}

fn runtime_terminal_host_preview_enabled(config: &brehon_types::RuntimeTerminalHostConfig) -> bool {
    runtime_terminal_host_preview_enabled_from_parts(
        config.preview_pane,
        std::env::var(EXPERIMENTAL_TERMINAL_HOST_PREVIEW_ENV)
            .ok()
            .as_deref(),
    )
}

fn runtime_terminal_host_preview_enabled_from_parts(
    config_preview_pane: Option<bool>,
    env_value: Option<&str>,
) -> bool {
    config_preview_pane.unwrap_or(false) || experimental_terminal_host_enabled_from_value(env_value)
}

fn eager_gateway_bootstrap_enabled(config: &brehon_types::RuntimeTerminalHostConfig) -> bool {
    eager_gateway_bootstrap_enabled_from_parts(
        config,
        std::env::var(EXPERIMENTAL_TERMINAL_HOST_PREVIEW_ENV)
            .ok()
            .as_deref(),
        std::env::var(EXPERIMENTAL_TERMINAL_HOST_EAGER_GATEWAY_BOOTSTRAP_ENV)
            .ok()
            .as_deref(),
    )
}

fn eager_gateway_bootstrap_enabled_from_parts(
    config: &brehon_types::RuntimeTerminalHostConfig,
    preview_env_value: Option<&str>,
    bootstrap_env_value: Option<&str>,
) -> bool {
    if let Some(enabled) = bool_env_value(bootstrap_env_value) {
        return enabled;
    }

    let preview_only_terminal_host = config.effective_kind()
        != brehon_types::RuntimeTerminalHostKind::Embedded
        && config.effective_pane_ownership() == brehon_types::RuntimeTerminalHostPaneOwnership::Mux
        && runtime_terminal_host_preview_enabled_from_parts(config.preview_pane, preview_env_value);

    !preview_only_terminal_host
}

fn experimental_terminal_host_enabled_from_value(value: Option<&str>) -> bool {
    matches!(bool_env_value(value), Some(true))
}

fn bool_env_value(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

async fn spawn_terminal_host_preview_pane(
    command_port: &Arc<dyn RuntimeCommandPort>,
    session_name: &str,
    cwd: &Path,
) -> std::result::Result<brehon_types::RuntimeCommandResult, PortError> {
    let marker = format!(
        "brehon-terminal-host-preview-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("preview")
    );
    command_port
        .execute(brehon_types::RuntimeCommand {
            command_id: format!("terminal-host-preview-{marker}"),
            target: brehon_types::RuntimeCommandTarget {
                session_id: session_name.to_string(),
                pane_id: None,
                generation: None,
            },
            issued_at_ms: runtime_now_ms(),
            kind: brehon_types::RuntimeCommandKind::SpawnPane {
                kind: brehon_types::RuntimePaneKind::Shell,
                pane_id: Some(TERMINAL_HOST_PREVIEW_PANE_ID.to_string()),
                title: Some("terminal host preview".to_string()),
                cwd: Some(cwd.display().to_string()),
                command: vec![
                    "sh".to_string(),
                    "-lc".to_string(),
                    format!("printf '%s\\n' {marker}; sleep 3600"),
                ],
                env: std::collections::BTreeMap::new(),
                rows: None,
                cols: None,
            },
        })
        .await
}

fn terminal_host_preview_pane_from_registry(
    registry: &brehon_daemon::PaneRegistrySnapshot,
    session_name: &str,
) -> Option<TerminalHostPreviewPane> {
    registry
        .panes
        .iter()
        .find(|pane| {
            pane.session_id == session_name && pane.pane_id == TERMINAL_HOST_PREVIEW_PANE_ID
        })
        .map(|pane| TerminalHostPreviewPane {
            pane_id: pane.pane_id.clone(),
            generation: pane.generation,
        })
}

async fn close_terminal_host_preview_pane(
    command_port: &Arc<dyn RuntimeCommandPort>,
    session_name: &str,
    pane: &TerminalHostPreviewPane,
) -> std::result::Result<brehon_types::RuntimeCommandResult, PortError> {
    command_port
        .execute(brehon_types::RuntimeCommand {
            command_id: format!(
                "terminal-host-preview-close-{}",
                uuid::Uuid::new_v4()
                    .to_string()
                    .split('-')
                    .next()
                    .unwrap_or("preview")
            ),
            target: brehon_types::RuntimeCommandTarget {
                session_id: session_name.to_string(),
                pane_id: Some(pane.pane_id.clone()),
                generation: Some(pane.generation),
            },
            issued_at_ms: runtime_now_ms(),
            kind: brehon_types::RuntimeCommandKind::ClosePane {
                reason: "brehon run shutdown".to_string(),
            },
        })
        .await
}

fn runtime_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn reset_runtime_dir(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn is_prompt_queue_root_file(path: &Path) -> bool {
    path.is_file()
        && !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        && (path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "prompt" | "entry"))
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".retry.json")))
}

fn dead_letter_stale_prompt_queue_root(prompt_queue_root: &Path) -> Result<usize> {
    if !prompt_queue_root.exists() {
        return Ok(0);
    }

    let dead_letter_dir = prompt_queue_root.join("dead-letter");
    let mut moved = 0usize;
    let mut entries = std::fs::read_dir(prompt_queue_root)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_prompt_queue_root_file(path))
        .collect::<Vec<_>>();
    entries.sort();

    for path in entries {
        std::fs::create_dir_all(&dead_letter_dir)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("queued.prompt");
        let target = dead_letter_dir.join(format!(
            "{:020}-startup-stale-{file_name}",
            chrono::Utc::now().timestamp_millis()
        ));
        std::fs::rename(&path, target)?;
        moved = moved.saturating_add(1);
    }

    Ok(moved)
}

fn prepare_runtime_session_state(cwd: &Path, session_name: &str) -> Result<usize> {
    let runtime_dir = cwd.join(".brehon").join("runtime");

    // These files are registrations for live child processes. Clear them
    // before spawning panes so fast-starting MCP children cannot register and
    // then be deleted by startup cleanup.
    reset_runtime_dir(&runtime_dir.join("sessions"))?;

    let prompt_queue_root_dir = runtime_dir.join("prompt-queue");
    std::fs::create_dir_all(&prompt_queue_root_dir)?;
    let stale_prompt_count = dead_letter_stale_prompt_queue_root(&prompt_queue_root_dir)?;
    reset_runtime_dir(&prompt_queue_root_dir.join(session_name))?;

    std::fs::write(
        runtime_dir.join("current-session.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "session_name": session_name,
            "written_at": chrono::Utc::now().to_rfc3339(),
        }))
        .unwrap_or_else(|_| "{}".to_string()),
    )?;

    reset_runtime_dir(&runtime_dir.join("reviewer-reset-queue"))?;
    reset_runtime_dir(&runtime_dir.join("reviewer-reset-acks"))?;
    reset_runtime_dir(&runtime_dir.join("agent-health"))?;

    Ok(stale_prompt_count)
}

fn terminal_host_startup_prompt_delay(slot: u64) -> std::time::Duration {
    std::time::Duration::from_secs(TERMINAL_HOST_STARTUP_PROMPT_DELAY_SECS)
        + std::time::Duration::from_millis(
            slot.saturating_mul(TERMINAL_HOST_STARTUP_PROMPT_STAGGER_MILLIS),
        )
}

fn combine_startup_policy(
    project_policy: Option<&str>,
    lane_system_prompt: Option<&str>,
) -> Option<String> {
    let mut blocks = Vec::new();
    if let Some(prompt) = lane_system_prompt
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        blocks.push(format!("Lane system prompt:\n{prompt}"));
    }
    if let Some(policy) = project_policy
        .map(str::trim)
        .filter(|policy| !policy.is_empty())
    {
        blocks.push(policy.to_string());
    }
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

fn seed_configured_advisor_rooms(
    brehon_root: &Path,
    config: &brehon_types::BrehonConfig,
    advisor_names: &[String],
    advisor_agent_type_map: &HashMap<String, String>,
) -> Result<()> {
    if !config.advisors.enabled {
        return Ok(());
    }

    let rooms_dir = brehon_root.join("runtime").join("advisors").join("rooms");
    std::fs::create_dir_all(&rooms_dir)?;
    for room in &config.advisors.rooms {
        let room_id = room.id.trim();
        if room_id.is_empty() {
            continue;
        }
        let path = rooms_dir.join(format!("{}.json", sanitize_advisor_room_id(room_id)));
        let mut value = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            serde_json::from_str::<serde_json::Value>(&content)?
        } else {
            serde_json::json!({
                "room_id": room_id,
                "created_at": chrono::Utc::now(),
                "messages": [],
            })
        };
        let object = value.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!("advisor room file {} is not a JSON object", path.display())
        })?;
        object.insert(
            "room_id".to_string(),
            serde_json::Value::String(room_id.to_string()),
        );
        object.insert(
            "title".to_string(),
            serde_json::Value::String(room.title.clone().unwrap_or_else(|| room_id.to_string())),
        );
        let turn_mode = room.turn_mode.unwrap_or(config.advisors.default_turn_mode);
        object.insert("turn_mode".to_string(), serde_json::to_value(turn_mode)?);
        object.insert(
            "participants".to_string(),
            serde_json::to_value(advisor_room_participants(
                config,
                room,
                advisor_names,
                advisor_agent_type_map,
            ))?,
        );
        object.insert("context".to_string(), serde_json::to_value(&room.context)?);
        object
            .entry("messages".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        object.insert(
            "updated_at".to_string(),
            serde_json::json!(chrono::Utc::now()),
        );

        let payload = serde_json::to_string_pretty(&value)?;
        let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        std::fs::write(&tmp, payload)?;
        std::fs::rename(&tmp, &path)?;
    }

    Ok(())
}

fn advisor_room_participants(
    config: &brehon_types::BrehonConfig,
    room: &brehon_types::AdvisorRoomConfig,
    advisor_names: &[String],
    advisor_agent_type_map: &HashMap<String, String>,
) -> Vec<String> {
    let declared_lanes: std::collections::HashSet<&str> =
        room.participants.iter().map(String::as_str).collect();
    let eligible_lanes: std::collections::HashSet<&str> = config
        .advisors
        .pools
        .iter()
        .filter(|pool| {
            (pool.rooms.is_empty() || pool.rooms.iter().any(|id| id == &room.id))
                && (declared_lanes.is_empty() || declared_lanes.contains(pool.lane.as_str()))
        })
        .map(|pool| pool.lane.as_str())
        .collect();

    advisor_names
        .iter()
        .filter(|name| {
            advisor_agent_type_map
                .get(*name)
                .is_some_and(|lane| eligible_lanes.contains(lane.as_str()))
        })
        .cloned()
        .collect()
}

fn sanitize_advisor_room_id(room_id: &str) -> String {
    room_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn reviewer_panel_terminal_host_maps(
    reviewer_panels: &[brehon_tui::ReviewerPanel],
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut panel_map = HashMap::new();
    let mut tab_map = HashMap::new();

    for (panel_idx, panel) in reviewer_panels.iter().enumerate() {
        let panel_name = normalized_reviewer_panel_name(&panel.name, panel_idx);
        let tab_name = if panel_idx == 0 {
            "Reviewers".to_string()
        } else {
            format!("Reviewers: {panel_name}")
        };
        for member in &panel.members {
            panel_map.insert(member.clone(), panel_name.clone());
            tab_map.insert(member.clone(), tab_name.clone());
        }
    }

    (panel_map, tab_map)
}

fn normalized_reviewer_panel_name(name: &str, panel_idx: usize) -> String {
    let normalized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_control() || ch == '\t' {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>();
    let normalized = normalized.trim();
    if normalized.is_empty() {
        format!("panel-{}", panel_idx + 1)
    } else {
        normalized.to_string()
    }
}

async fn dispatch_terminal_host_startup_prompts(
    router: Arc<dyn RuntimeCommandRouter>,
    session_name: String,
    mut prompts: Vec<TerminalHostStartupPrompt>,
) {
    prompts.sort_by_key(|prompt| prompt.delay);
    let started_at = tokio::time::Instant::now();

    for prompt in prompts {
        let elapsed = started_at.elapsed();
        if prompt.delay > elapsed {
            tokio::time::sleep(prompt.delay - elapsed).await;
        }

        let target = prompt.target.clone();
        let prompt_id = uuid::Uuid::new_v4().to_string();
        let command = brehon_types::RuntimeCommand {
            command_id: format!("terminal-host-startup-{prompt_id}"),
            target: brehon_types::RuntimeCommandTarget {
                session_id: session_name.clone(),
                pane_id: Some(target.clone()),
                generation: None,
            },
            issued_at_ms: runtime_now_ms(),
            kind: brehon_types::RuntimeCommandKind::SendPrompt {
                prompt_id,
                text: prompt.prompt,
                from: None,
                delivery: brehon_types::PromptDeliveryMode::Direct,
            },
        };

        match router
            .route_command(command, brehon_types::RuntimePolicyContext::default())
            .await
        {
            Ok(result) if result.status == brehon_types::RuntimeCommandStatus::Applied => {
                tracing::info!(target = %target, "Delivered terminal-host startup prompt");
            }
            Ok(result) => {
                tracing::warn!(
                    target = %target,
                    status = ?result.status,
                    message = ?result.message,
                    "Terminal-host startup prompt was not applied"
                );
            }
            Err(err) => {
                tracing::warn!(
                    target = %target,
                    error = %err,
                    "Failed to deliver terminal-host startup prompt"
                );
            }
        }
    }
}

fn build_team_member_cwds(
    worker_cwds: &HashMap<String, PathBuf>,
    reviewer_cwds: &HashMap<String, PathBuf>,
    advisor_cwds: &HashMap<String, PathBuf>,
    research_cwds: &HashMap<String, PathBuf>,
    supervisor_name: &str,
    supervisor_cwds: &HashMap<String, PathBuf>,
) -> HashMap<String, PathBuf> {
    let mut team_member_cwds = worker_cwds.clone();
    team_member_cwds.extend(reviewer_cwds.clone());
    team_member_cwds.extend(advisor_cwds.clone());
    team_member_cwds.extend(research_cwds.clone());
    if let Some(supervisor_cwd) = supervisor_cwds.get(supervisor_name) {
        team_member_cwds.insert(supervisor_name.to_string(), supervisor_cwd.clone());
    }
    team_member_cwds
}

struct RuntimeTerminalHostWiring {
    runtime_terminal_host: Option<brehon_host::ConfiguredTerminalHost>,
    terminal_host_event_forwarder: Arc<LateBindingRuntimeEventSink>,
    runtime_command_port: Arc<dyn RuntimeCommandPort>,
    runtime_command_rx: Option<brehon_mux::MuxRuntimeCommandReceiver>,
    #[allow(dead_code)]
    terminal_host_command_port: Option<Arc<dyn RuntimeCommandPort>>,
    terminal_host_status: brehon_daemon::RuntimeTerminalHostStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeTerminalHostAgentFactoryLaunchReport {
    launched: usize,
    results: Vec<brehon_types::RuntimeCommandResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeTerminalHostWiringSmokeReport {
    pub terminal_host_status: brehon_daemon::RuntimeTerminalHostStatus,
    pub spawn_status: brehon_types::RuntimeCommandStatus,
    pub resize_status: brehon_types::RuntimeCommandStatus,
    pub input_status: brehon_types::RuntimeCommandStatus,
    pub reset_status: brehon_types::RuntimeCommandStatus,
    pub stale_input_status: brehon_types::RuntimeCommandStatus,
    pub post_reset_input_status: brehon_types::RuntimeCommandStatus,
    pub prompt_status: brehon_types::RuntimeCommandStatus,
    pub observed_output: bool,
    pub close_status: brehon_types::RuntimeCommandStatus,
    pub post_close_status: brehon_types::RuntimeCommandStatus,
    pub registry_count: usize,
}

fn build_runtime_terminal_host_wiring(
    config: &brehon_types::RuntimeTerminalHostConfig,
    session_name: &str,
) -> Result<RuntimeTerminalHostWiring> {
    let runtime_terminal_host =
        brehon_host::configured_terminal_host_from_runtime_config(config, session_name)?;
    let kind = config.effective_kind();
    let pane_ownership = config.effective_pane_ownership();
    if pane_ownership == brehon_types::RuntimeTerminalHostPaneOwnership::Host
        && runtime_terminal_host.is_none()
    {
        anyhow::bail!(
            "runtime.terminal_host.pane_ownership=host requires an adapter-backed terminal host; got {:?}",
            kind,
        );
    }
    let terminal_host_event_forwarder = Arc::new(LateBindingRuntimeEventSink::default());
    let (mux_runtime_command_port, runtime_command_rx) =
        brehon_mux::MuxRuntimeCommandPort::channel_default();
    let mux_runtime_command_port: Arc<dyn RuntimeCommandPort> = Arc::new(mux_runtime_command_port);
    let terminal_host_command_port: Option<Arc<dyn RuntimeCommandPort>> =
        runtime_terminal_host.as_ref().map(|host| {
            let command_port = brehon_host::TerminalHostCommandPort::new(host.adapter())
                .with_event_sink(terminal_host_event_forwarder.clone());
            Arc::new(command_port) as Arc<dyn RuntimeCommandPort>
        });
    let (runtime_command_port, runtime_command_rx, command_routing) = if pane_ownership
        == brehon_types::RuntimeTerminalHostPaneOwnership::Host
    {
        let command_port = terminal_host_command_port.clone().ok_or_else(|| {
            anyhow::anyhow!("host-owned runtime command routing requires an adapter-backed host")
        })?;
        (
            command_port,
            None,
            brehon_daemon::RuntimeTerminalHostCommandRouting::TerminalHost,
        )
    } else {
        (
            mux_runtime_command_port,
            Some(runtime_command_rx),
            brehon_daemon::RuntimeTerminalHostCommandRouting::Mux,
        )
    };
    let terminal_host_identity = runtime_terminal_host
        .as_ref()
        .map(|host| host.runtime_identity(session_name))
        .unwrap_or_default();
    let terminal_host_capabilities = runtime_terminal_host
        .as_ref()
        .map(|host| host.adapter().capabilities());
    let mut terminal_host_status = brehon_daemon::RuntimeTerminalHostStatus {
        kind,
        experimental: runtime_terminal_host.is_some(),
        observation_running: runtime_terminal_host
            .as_ref()
            .and_then(|host| host.observer())
            .is_some(),
        command_routing,
        pane_ownership,
        agent_factory: brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::Mux,
        capabilities: terminal_host_capabilities,
        promotion_readiness: brehon_daemon::RuntimeTerminalHostPromotionReadiness::default(),
        session_name: terminal_host_identity.session_name,
        socket_name: terminal_host_identity.socket_name,
        socket_dir: terminal_host_identity.socket_dir,
        binary_path: terminal_host_identity.binary_path,
        diagnostics: Vec::new(),
    };
    terminal_host_status.promotion_readiness =
        brehon_daemon::terminal_host_promotion_readiness(Some(&terminal_host_status));
    Ok(RuntimeTerminalHostWiring {
        runtime_terminal_host,
        terminal_host_event_forwarder,
        runtime_command_port,
        runtime_command_rx,
        terminal_host_command_port,
        terminal_host_status,
    })
}

fn terminal_host_status_with_agent_factory_plan_and_owner(
    mut status: brehon_daemon::RuntimeTerminalHostStatus,
    plan: &brehon_mux::TerminalHostAgentFactoryPlan,
    owner: brehon_daemon::RuntimeTerminalHostAgentFactoryRouting,
) -> brehon_daemon::RuntimeTerminalHostStatus {
    status.agent_factory = owner;
    status.promotion_readiness = brehon_daemon::terminal_host_promotion_readiness(Some(&status));
    if !plan.blocked_panes.is_empty() {
        let pane_label = if plan.blocked_panes.len() == 1 {
            "pane is"
        } else {
            "panes are"
        };
        status.promotion_readiness.blockers.push(format!(
            "{} of {} mux-created {pane_label} not terminal-host PTY eligible",
            plan.blocked_panes.len(),
            plan.total_panes
        ));
        for blocked in plan.blocked_panes.iter().take(5) {
            status.promotion_readiness.blockers.push(format!(
                "{} '{}' is not host-eligible: {}",
                blocked.kind, blocked.pane_id, blocked.reason
            ));
        }
        if plan.blocked_panes.len() > 5 {
            status.promotion_readiness.blockers.push(format!(
                "... {} additional pane(s) are not host-eligible",
                plan.blocked_panes.len() - 5
            ));
        }
        status.promotion_readiness.ready = false;
    }
    status
}

async fn launch_terminal_host_agent_factory_plan(
    daemon: &brehon_daemon::RuntimeDaemon,
    session_name: &str,
    plan: &brehon_mux::TerminalHostAgentFactoryPlan,
) -> Result<RuntimeTerminalHostAgentFactoryLaunchReport> {
    launch_terminal_host_agent_factory_plan_with_progress(daemon, session_name, plan, |_, _, _| {})
        .await
}

async fn launch_terminal_host_agent_factory_plan_with_progress<F>(
    daemon: &brehon_daemon::RuntimeDaemon,
    session_name: &str,
    plan: &brehon_mux::TerminalHostAgentFactoryPlan,
    mut on_progress: F,
) -> Result<RuntimeTerminalHostAgentFactoryLaunchReport>
where
    F: FnMut(usize, usize, &brehon_mux::AgentTerminalLaunchSpec),
{
    if !plan.ready() {
        anyhow::bail!(
            "terminal-host agent factory plan is not ready: {} launchable of {} pane(s), {} blocked",
            plan.launch_specs.len(),
            plan.total_panes,
            plan.blocked_panes.len()
        );
    }

    let mut results = Vec::with_capacity(plan.launch_specs.len());
    let total = plan.launch_specs.len();
    for (idx, launch) in plan.launch_specs.iter().enumerate() {
        if launch.spec.session_id != session_name {
            anyhow::bail!(
                "terminal-host agent launch '{}' targets session '{}', expected '{}'",
                launch.spec.pane_id,
                launch.spec.session_id,
                session_name
            );
        }
        on_progress(idx.saturating_add(1), total, launch);
        let pane_id = launch.spec.pane_id.as_str();
        let result = daemon
            .route_command(
                terminal_host_agent_factory_spawn_command(session_name, pane_id, launch),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        ensure_runtime_command_applied(&result, "terminal-host agent factory spawn")?;
        results.push(result);
    }

    Ok(RuntimeTerminalHostAgentFactoryLaunchReport {
        launched: results.len(),
        results,
    })
}

fn terminal_host_agent_factory_spawn_command(
    session_id: &str,
    pane_id: &str,
    launch: &brehon_mux::AgentTerminalLaunchSpec,
) -> brehon_types::RuntimeCommand {
    brehon_types::RuntimeCommand {
        command_id: format!(
            "terminal-host-agent-factory-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("spawn")
        ),
        target: brehon_types::RuntimeCommandTarget {
            session_id: session_id.to_string(),
            pane_id: Some(pane_id.to_string()),
            generation: None,
        },
        issued_at_ms: runtime_now_ms(),
        kind: launch.to_runtime_spawn_command(),
    }
}

pub(crate) async fn run_runtime_terminal_host_wiring_smoke(
    config: &brehon_types::RuntimeTerminalHostConfig,
    session_name: &str,
    cwd: &Path,
) -> Result<RuntimeTerminalHostWiringSmokeReport> {
    let RuntimeTerminalHostWiring {
        runtime_terminal_host,
        terminal_host_event_forwarder,
        runtime_command_port,
        runtime_command_rx,
        terminal_host_command_port,
        terminal_host_status,
    } = build_runtime_terminal_host_wiring(config, session_name)?;

    if runtime_command_rx.is_some() {
        anyhow::bail!(
            "run wiring smoke requires host-owned daemon command routing; got {:?}",
            terminal_host_status.command_routing
        );
    }
    if terminal_host_command_port.is_none() {
        anyhow::bail!("run wiring smoke requires a terminal-host command port");
    }
    if terminal_host_status.command_routing
        != brehon_daemon::RuntimeTerminalHostCommandRouting::TerminalHost
    {
        anyhow::bail!(
            "run wiring smoke requires terminal-host command routing; got {:?}",
            terminal_host_status.command_routing
        );
    }
    if terminal_host_status.pane_ownership != brehon_types::RuntimeTerminalHostPaneOwnership::Host {
        anyhow::bail!(
            "run wiring smoke requires host pane ownership; got {:?}",
            terminal_host_status.pane_ownership
        );
    }
    let terminal_host_supports_absolute_resize = terminal_host_status
        .capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.absolute_resize);

    let configured_host =
        runtime_terminal_host.ok_or_else(|| anyhow::anyhow!("run wiring smoke requires a host"))?;
    let pane_id = "run-wiring-worker";
    let mut launch_specs = Vec::new();
    for (launch_pane_id, kind, title) in [
        (
            "run-wiring-supervisor",
            brehon_types::RuntimePaneKind::Supervisor,
            "runtime host wiring supervisor",
        ),
        (
            pane_id,
            brehon_types::RuntimePaneKind::Worker,
            "runtime host wiring worker",
        ),
        (
            "run-wiring-reviewer",
            brehon_types::RuntimePaneKind::Reviewer,
            "runtime host wiring reviewer",
        ),
    ] {
        let launch = match brehon_mux::AgentTerminalLaunchPlan::from_pty_config(
            session_name,
            launch_pane_id,
            Some(title.to_string()),
            kind,
            &brehon_mux::PtyConfig {
                command: "sh".to_string(),
                args: vec!["-lc".to_string(), "cat".to_string()],
                cwd: Some(cwd.to_path_buf()),
                env: vec![("BREHON_TEST_PANE".to_string(), launch_pane_id.to_string())],
                rows: 24,
                cols: 80,
            },
        ) {
            brehon_mux::AgentTerminalLaunchPlan::TerminalHost(launch) => launch,
            _ => unreachable!("pty config launch plans are terminal-host eligible"),
        };
        launch_specs.push(launch);
    }
    let agent_factory_plan = brehon_mux::TerminalHostAgentFactoryPlan {
        total_panes: 3,
        launch_specs,
        blocked_panes: Vec::new(),
    };
    let terminal_host_status = terminal_host_status_with_agent_factory_plan_and_owner(
        terminal_host_status,
        &agent_factory_plan,
        brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::TerminalHost,
    );
    let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
        policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
        command_port: Some(runtime_command_port),
        terminal_host: Some(terminal_host_status.clone()),
        ..brehon_daemon::RuntimeDaemonConfig::default()
    });
    let runtime_event_sink: Arc<dyn RuntimeEventSink> = Arc::new(daemon.clone());
    terminal_host_event_forwarder.bind(runtime_event_sink);

    let result = async {
        let factory_report =
            launch_terminal_host_agent_factory_plan(&daemon, session_name, &agent_factory_plan)
                .await?;
        if factory_report.launched != 3 {
            anyhow::bail!(
                "run wiring smoke launched {} pane(s), expected 3",
                factory_report.launched
            );
        }
        let spawn = factory_report
            .results
            .iter()
            .find(|result| {
                result
                    .command_id
                    .starts_with("terminal-host-agent-factory-")
            })
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("run wiring smoke did not record spawn result"))?;
        let brehon_host::ConfiguredTerminalHost::Headless(host) = &configured_host;
        let snapshot = host
            .snapshot(session_name, pane_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("run wiring smoke headless pane missing"))?;
        if snapshot.rows != 24 || snapshot.cols != 80 {
            anyhow::bail!(
                "run wiring smoke spawn dimensions were {}x{}, expected 80x24",
                snapshot.cols,
                snapshot.rows
            );
        }
        let expected_command = vec!["sh".to_string(), "-lc".to_string(), "cat".to_string()];
        if snapshot.command != expected_command {
            anyhow::bail!(
                "run wiring smoke spawn command mismatch: {:?}",
                snapshot.command
            );
        }

        let resize = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(1),
                    brehon_types::RuntimeCommandKind::ResizePane {
                        rows: 30,
                        cols: 100,
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        if terminal_host_supports_absolute_resize {
            ensure_runtime_host_smoke_applied(&resize, "resize")?;
        } else if resize.status != brehon_types::RuntimeCommandStatus::Rejected {
            anyhow::bail!(
                "run wiring smoke expected unsupported resize to be rejected, got {:?}: {}",
                resize.status,
                resize.message.as_deref().unwrap_or("no result message")
            );
        }

        let input = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(1),
                    brehon_types::RuntimeCommandKind::SendTerminalInput {
                        bytes: b"brehon-run-wiring-smoke\n".to_vec(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        ensure_runtime_host_smoke_applied(&input, "input")?;

        let reset = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(1),
                    brehon_types::RuntimeCommandKind::ResetPane {
                        reason: "run wiring smoke reset".to_string(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        ensure_runtime_host_smoke_applied(&reset, "reset")?;

        let stale_input = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(1),
                    brehon_types::RuntimeCommandKind::SendTerminalInput {
                        bytes: b"stale generation".to_vec(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        if stale_input.status != brehon_types::RuntimeCommandStatus::Rejected {
            anyhow::bail!(
                "expected stale-generation input to be rejected after reset, got {:?}",
                stale_input.status
            );
        }

        let post_reset_marker = "brehon-run-wiring-smoke-after-reset";
        let post_reset_input = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(2),
                    brehon_types::RuntimeCommandKind::SendTerminalInput {
                        bytes: format!("{post_reset_marker}\n").into_bytes(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        ensure_runtime_host_smoke_applied(&post_reset_input, "post-reset input")?;
        let observed_output = ensure_runtime_host_wiring_output_observed(
            &configured_host,
            &daemon,
            session_name,
            pane_id,
            2,
            post_reset_marker,
        )
        .await?;

        let prompt = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(2),
                    brehon_types::RuntimeCommandKind::SendPrompt {
                        prompt_id: "run-wiring-smoke-prompt".to_string(),
                        text: "brehon run wiring smoke prompt\n".to_string(),
                        from: None,
                        delivery: brehon_types::PromptDeliveryMode::Direct,
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        ensure_runtime_host_smoke_applied(&prompt, "prompt")?;

        let close = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(2),
                    brehon_types::RuntimeCommandKind::ClosePane {
                        reason: "run wiring smoke complete".to_string(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        ensure_runtime_host_smoke_applied(&close, "close")?;

        let post_close = daemon
            .route_command(
                runtime_terminal_host_wiring_smoke_command(
                    session_name,
                    pane_id,
                    Some(2),
                    brehon_types::RuntimeCommandKind::SendTerminalInput {
                        bytes: b"after close".to_vec(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await?;
        if post_close.status != brehon_types::RuntimeCommandStatus::Rejected {
            anyhow::bail!(
                "expected post-close input to be rejected, got {:?}",
                post_close.status
            );
        }

        let registry = daemon.pane_registry_snapshot().await;
        let pane = registry
            .panes
            .iter()
            .find(|pane| pane.session_id == session_name && pane.pane_id == pane_id)
            .ok_or_else(|| anyhow::anyhow!("run wiring smoke pane missing from daemon registry"))?;
        if pane.state != brehon_types::RuntimePaneState::Dead {
            anyhow::bail!(
                "run wiring smoke pane should be dead after close, got {:?}",
                pane.state
            );
        }
        if pane.generation != 2 {
            anyhow::bail!(
                "run wiring smoke pane should be generation 2 after reset, got {}",
                pane.generation
            );
        }

        Ok(RuntimeTerminalHostWiringSmokeReport {
            terminal_host_status: terminal_host_status.clone(),
            spawn_status: spawn.status,
            resize_status: resize.status,
            input_status: input.status,
            reset_status: reset.status,
            stale_input_status: stale_input.status,
            post_reset_input_status: post_reset_input.status,
            prompt_status: prompt.status,
            observed_output,
            close_status: close.status,
            post_close_status: post_close.status,
            registry_count: registry.panes.len(),
        })
    }
    .await;

    daemon.shutdown().await;
    let cleanup = configured_host.shutdown().await;
    if let Err(err) = cleanup {
        if result.is_ok() {
            anyhow::bail!("run wiring smoke cleanup failed: {err}");
        }
        tracing::warn!(error = %err, "run wiring smoke cleanup failed after probe error");
    }
    result
}

fn ensure_runtime_host_smoke_applied(
    result: &brehon_types::RuntimeCommandResult,
    operation: &str,
) -> Result<()> {
    let context = format!("run wiring smoke {operation}");
    ensure_runtime_command_applied(result, &context)
}

fn ensure_runtime_command_applied(
    result: &brehon_types::RuntimeCommandResult,
    operation: &str,
) -> Result<()> {
    if result.status == brehon_types::RuntimeCommandStatus::Applied {
        return Ok(());
    }
    anyhow::bail!(
        "{operation} failed with {:?}: {}",
        result.status,
        result.message.as_deref().unwrap_or("no result message")
    )
}

async fn ensure_runtime_host_wiring_output_observed(
    configured_host: &brehon_host::ConfiguredTerminalHost,
    daemon: &brehon_daemon::RuntimeDaemon,
    session_name: &str,
    pane_id: &str,
    generation: u64,
    marker: &str,
) -> Result<bool> {
    let brehon_host::ConfiguredTerminalHost::Headless(host) = configured_host;
    let snapshot = host
        .snapshot(session_name, pane_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("run wiring smoke headless pane missing"))?;
    if String::from_utf8_lossy(&snapshot.input_bytes).contains(marker) {
        return Ok(true);
    }
    let _ = (daemon, session_name, pane_id, generation);
    anyhow::bail!("run wiring smoke headless pane did not record post-reset marker input");
}

fn runtime_terminal_host_wiring_smoke_command(
    session_id: &str,
    pane_id: &str,
    generation: Option<u64>,
    kind: brehon_types::RuntimeCommandKind,
) -> brehon_types::RuntimeCommand {
    brehon_types::RuntimeCommand {
        command_id: format!(
            "run-wiring-smoke-{}",
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
        issued_at_ms: runtime_now_ms(),
        kind,
    }
}

pub async fn execute(
    project_path: Option<&Path>,
    config_override: Option<&Path>,
    workers_override: Option<&str>,
) -> Result<()> {
    use brehon_config::load_config_with_override;
    use brehon_mux::{AgentAdapter, HarnessControlPlane, Mux, MuxConfig, SupervisorCli};

    let raw_cwd = project_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let cwd = normalize_project_root(&raw_cwd);
    let mut splash = crate::ui::StartupSplash::new();
    splash.set_stage("Loading configuration");
    splash.record(format!("Project root: {}", cwd.display()));

    let config = load_config_with_override(Some(&cwd), config_override)?;
    ensure_runtime_terminal_host_supported(&config)?;
    let eager_gateway_bootstrap = eager_gateway_bootstrap_enabled(&config.runtime.terminal_host);
    let runtime_worktree_root = effective_worktree_root(&cwd, &config);

    let session_name = format!(
        "brehon-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("session")
    );

    std::env::set_var("BREHON_ROOT", cwd.join(".brehon"));
    std::env::set_var("BREHON_PROJECT_ROOT", &cwd);
    std::env::set_var("BREHON_WORKSPACE_ROOT", &cwd);
    std::env::set_var("BREHON_WORKTREE_ROOT", &runtime_worktree_root);
    std::env::set_var("BREHON_SESSION_NAME", &session_name);

    splash.set_stage("Preparing runtime");
    match terminate_project_processes(Some(&cwd), &[], true) {
        Ok(survivors) if survivors.is_empty() => {}
        Ok(survivors) => {
            return Err(anyhow::anyhow!(
                "failed to clear stale Brehon processes for this project: {}",
                survivors
                    .into_iter()
                    .map(|pid| pid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to reap stale Brehon processes before startup: {err}"
            ));
        }
    }

    let shared_root_default_branch = if config.orchestration.worktree_isolation {
        splash.set_stage("Preparing isolated worktrees");
        splash.record("Checking shared repository branch".to_string());
        let default_branch = ensure_shared_root_on_default_branch(&cwd)?;
        splash.record(format!(
            "Installing protected branch guard for '{default_branch}'"
        ));
        ensure_protected_branch_hooks(&cwd, &default_branch)?;
        Some(default_branch)
    } else {
        None
    };
    let _protected_branch_guard_activation = if shared_root_default_branch.is_some() {
        Some(activate_protected_branch_guard(&cwd, &session_name)?)
    } else {
        None
    };

    // Ensure MCP server config exists before spawning agents
    splash.set_stage("Preparing runtime");
    splash.record("Ensuring MCP configuration".to_string());
    if let Err(e) = ensure_mcp_config(&cwd) {
        tracing::warn!("Failed to ensure MCP config: {:?}", e);
        splash.record(format!("MCP configuration warning: {e}"));
    }
    // Install the Claude PreToolUse worktree-containment hook. Tied to the
    // session lifetime via `_claude_hook_activation` below — when Brehon
    // exits (or panics), Drop removes the active marker and the hook turns
    // into a no-op until the next `brehon run`.
    splash.record("Installing Claude worktree-containment hook".to_string());
    if let Err(e) = ensure_claude_worktree_hook(&cwd) {
        tracing::warn!("Failed to install Claude worktree hook: {:?}", e);
        splash.record(format!("Claude hook install warning: {e}"));
    }
    let _claude_hook_activation = match activate_claude_worktree_hook(&cwd) {
        Ok(activation) => Some(activation),
        Err(e) => {
            tracing::warn!("Failed to activate Claude worktree hook: {:?}", e);
            None
        }
    };
    splash.record("Ensuring Codex instruction files".to_string());
    ensure_codex_instruction_files(&cwd, &config)?;

    splash.record("Reconciling initiative hierarchy".to_string());
    match reconcile_initiative_hierarchy_for_run(&cwd, &cwd.join(".brehon"), &config).await {
        Ok(repaired) => {
            for message in repaired {
                tracing::info!(message = %message, "Reconciled initiative hierarchy during startup");
                splash.record(message);
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "Failed to reconcile initiative hierarchy during startup");
            splash.record(format!("Initiative reconciliation warning: {err}"));
        }
    }

    // Generate memorable names for all agents
    let worker_pool_counts = resolve_worker_pool_counts(&config, workers_override)?;
    let total_workers: usize = worker_pool_counts.iter().map(|count| *count as usize).sum();
    let total_reviewers: usize = config.roles.reviewers.iter().map(|p| p.min as usize).sum();
    let total_advisors: usize = if config.advisors.enabled {
        config.advisors.pools.iter().map(|p| p.min as usize).sum()
    } else {
        0
    };
    let total_researchers: usize = if config.research.enabled {
        config.research.pools.iter().map(|p| p.min as usize).sum()
    } else {
        0
    };
    splash.set_summary(format!(
        "{} workers, {} reviewers, {} advisors, {} researchers, supervisor {}",
        total_workers,
        total_reviewers,
        total_advisors,
        total_researchers,
        config.roles.supervisor.name
    ));
    // Publish the structured roster so the splash architecture diagram can
    // display per-kind breakdowns (e.g. "claude 2  codex 1") instead of just
    // a total agent count.
    let worker_roster: Vec<(String, u32)> = config
        .roles
        .workers
        .iter()
        .zip(worker_pool_counts.iter())
        .map(|(pool, count)| (pool.lane.clone(), *count))
        .collect();
    let reviewer_roster: Vec<(String, u32)> = config
        .roles
        .reviewers
        .iter()
        .map(|pool| (pool.lane.clone(), pool.min))
        .collect();
    splash.set_roster(
        worker_roster,
        reviewer_roster,
        config.roles.supervisor.name.clone(),
    );
    splash.record(format!(
        "Planned launch: {} workers, {} reviewers, {} advisors, {} researchers, supervisor {}",
        total_workers,
        total_reviewers,
        total_advisors,
        total_researchers,
        config.roles.supervisor.name
    ));
    let generated_names = crate::names::generate_names(
        total_workers + total_reviewers + total_advisors + total_researchers,
    );
    let mut name_iter = generated_names.into_iter();

    // Build worker names and per-worker adapter map
    let mut worker_names = Vec::new();
    let mut worker_cli_map = std::collections::HashMap::new();
    let mut worker_agent_type_map = std::collections::HashMap::new();
    let mut worker_count = 0usize;

    // Build the model string for an agent. OpenCode uses provider/name format
    // (e.g. "ollama-cloud/glm-5.1"), all others use just the model name.
    let format_model = |launcher: &str, provider: &str, model_name: &str| -> String {
        if launcher == "opencode" {
            format!("{provider}/{model_name}")
        } else {
            model_name.to_string()
        }
    };
    let worktree_root_env_value = runtime_worktree_root.to_string_lossy().to_string();
    let launcher_env_pairs = |lane: &str| -> Vec<(String, String)> {
        let mut pairs = config
            .lane_launcher(lane)
            .map(|launcher| {
                launcher
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        pairs.retain(|(key, _)| key != "BREHON_WORKTREE_ROOT");
        pairs.push((
            "BREHON_WORKTREE_ROOT".to_string(),
            worktree_root_env_value.clone(),
        ));
        pairs.sort_by(|left, right| left.0.cmp(&right.0));
        pairs
    };

    let mut worker_model_map = std::collections::HashMap::new();
    let mut worker_reasoning_effort_map = std::collections::HashMap::new();
    let mut worker_env_map = std::collections::HashMap::new();
    let mut worker_startup_policy_map = std::collections::HashMap::new();
    let worker_project_policy = config.project_prompt_for_role_name("worker");
    for (pool, pool_count) in config.roles.workers.iter().zip(worker_pool_counts.iter()) {
        let model = config
            .lane_model(&pool.lane, pool.model.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!("Worker lane '{}' has no model configured", pool.lane)
            })?;
        let reasoning_effort =
            config.lane_reasoning_effort(&pool.lane, pool.reasoning_effort.as_deref());
        let launcher_name = config
            .lane_launcher_name(&pool.lane)
            .unwrap_or(pool.lane.as_str());
        let model_str = format_model(launcher_name, &model.provider, &model.name);
        for i in 0..*pool_count {
            let name = name_iter
                .next()
                .unwrap_or_else(|| format!("{}-{}", pool.lane, i + 1));
            worker_cli_map.insert(name.clone(), agent_to_adapter(&pool.lane, &config));
            worker_agent_type_map.insert(name.clone(), pool.lane.clone());
            worker_model_map.insert(name.clone(), model_str.clone());
            let mut worker_env = launcher_env_pairs(&pool.lane);
            if let Some(policy) = combine_startup_policy(
                worker_project_policy.as_deref(),
                config.lane_system_prompt(&pool.lane, None),
            ) {
                worker_env.push(("BREHON_ROLE_SYSTEM_PROMPT".to_string(), policy.clone()));
                worker_startup_policy_map.insert(name.clone(), policy);
            }
            worker_env_map.insert(name.clone(), worker_env);
            if let Some(effort) = reasoning_effort {
                worker_reasoning_effort_map.insert(name.clone(), effort.to_string());
            }
            worker_names.push(name);
            worker_count += 1;
        }
    }

    // Build reviewer names, per-reviewer adapter map, and panel groupings.
    //
    // A review panel is a group of reviewers that review the same work together.
    // Each panel can contain agents of different types (codex + gemini + claude).
    // The number of panels = max(pool.min) across all pools.
    // Panel N gets the Nth reviewer from each pool (if it exists).
    let mut reviewer_names = Vec::new();
    let mut reviewer_cli_map = std::collections::HashMap::new();
    let mut reviewer_agent_type_map = std::collections::HashMap::new();

    // First pass: spawn all reviewer panes and collect per-pool name lists
    let mut pool_reviewer_names: Vec<Vec<String>> = Vec::new();
    let mut reviewer_model_map = std::collections::HashMap::new();
    let mut reviewer_reasoning_effort_map = std::collections::HashMap::new();
    let mut reviewer_env_map = std::collections::HashMap::new();
    let mut reviewer_startup_policy_map = std::collections::HashMap::new();
    let reviewer_project_policy = config.project_prompt_for_role_name("reviewer");
    for pool in &config.roles.reviewers {
        let model = config
            .lane_model(&pool.lane, pool.model.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!("Reviewer lane '{}' has no model configured", pool.lane)
            })?;
        let reasoning_effort =
            config.lane_reasoning_effort(&pool.lane, pool.reasoning_effort.as_deref());
        let launcher_name = config
            .lane_launcher_name(&pool.lane)
            .unwrap_or(pool.lane.as_str());
        let model_str = format_model(launcher_name, &model.provider, &model.name);
        let mut names_in_pool = Vec::new();
        for i in 0..pool.min {
            let name = name_iter
                .next()
                .unwrap_or_else(|| format!("{}-reviewer-{}", pool.lane, i + 1));
            reviewer_cli_map.insert(name.clone(), agent_to_adapter(&pool.lane, &config));
            reviewer_agent_type_map.insert(name.clone(), pool.lane.clone());
            reviewer_model_map.insert(name.clone(), model_str.clone());
            let mut reviewer_env = launcher_env_pairs(&pool.lane);
            if let Some(policy) = combine_startup_policy(
                reviewer_project_policy.as_deref(),
                config.lane_system_prompt(&pool.lane, pool.system_prompt.as_deref()),
            ) {
                reviewer_env.push(("BREHON_ROLE_SYSTEM_PROMPT".to_string(), policy.clone()));
                reviewer_startup_policy_map.insert(name.clone(), policy);
            }
            reviewer_env_map.insert(name.clone(), reviewer_env);
            if let Some(effort) = reasoning_effort {
                reviewer_reasoning_effort_map.insert(name.clone(), effort.to_string());
            }
            reviewer_names.push(name.clone());
            names_in_pool.push(name);
        }
        pool_reviewer_names.push(names_in_pool);
    }

    let mut advisor_names = Vec::new();
    let mut advisor_cli_map = std::collections::HashMap::new();
    let mut advisor_agent_type_map = std::collections::HashMap::new();
    let mut advisor_model_map = std::collections::HashMap::new();
    let mut advisor_reasoning_effort_map = std::collections::HashMap::new();
    let mut advisor_env_map = std::collections::HashMap::new();
    let mut advisor_startup_policy_map = std::collections::HashMap::new();
    let advisor_project_policy = config.project_prompt_for_role_name("advisor");
    if config.advisors.enabled {
        for pool in &config.advisors.pools {
            let model = config
                .lane_model(&pool.lane, pool.model.as_ref())
                .ok_or_else(|| {
                    anyhow::anyhow!("Advisor lane '{}' has no model configured", pool.lane)
                })?;
            let reasoning_effort =
                config.lane_reasoning_effort(&pool.lane, pool.reasoning_effort.as_deref());
            let launcher_name = config
                .lane_launcher_name(&pool.lane)
                .unwrap_or(pool.lane.as_str());
            let model_str = format_model(launcher_name, &model.provider, &model.name);
            for i in 0..pool.min {
                let name = name_iter
                    .next()
                    .unwrap_or_else(|| format!("{}-advisor-{}", pool.lane, i + 1));
                advisor_cli_map.insert(name.clone(), agent_to_adapter(&pool.lane, &config));
                advisor_agent_type_map.insert(name.clone(), pool.lane.clone());
                advisor_model_map.insert(name.clone(), model_str.clone());
                let mut advisor_env = launcher_env_pairs(&pool.lane);
                if let Some(policy) = combine_startup_policy(
                    advisor_project_policy.as_deref(),
                    config.lane_system_prompt(&pool.lane, pool.system_prompt.as_deref()),
                ) {
                    advisor_env.push(("BREHON_ROLE_SYSTEM_PROMPT".to_string(), policy.clone()));
                    advisor_startup_policy_map.insert(name.clone(), policy);
                }
                advisor_env_map.insert(name.clone(), advisor_env);
                if let Some(effort) = reasoning_effort {
                    advisor_reasoning_effort_map.insert(name.clone(), effort.to_string());
                }
                advisor_names.push(name);
            }
        }
    }

    let mut research_names = Vec::new();
    let mut research_cli_map = std::collections::HashMap::new();
    let mut research_agent_type_map = std::collections::HashMap::new();
    let mut research_model_map = std::collections::HashMap::new();
    let mut research_reasoning_effort_map = std::collections::HashMap::new();
    let mut research_env_map = std::collections::HashMap::new();
    let mut research_startup_policy_map = std::collections::HashMap::new();
    let research_project_policy = config.project_prompt_for_role_name("research");
    if config.research.enabled {
        for pool in &config.research.pools {
            let model = config.lane_model(&pool.lane, None).ok_or_else(|| {
                anyhow::anyhow!("Research lane '{}' has no model configured", pool.lane)
            })?;
            let reasoning_effort = config.lane_reasoning_effort(&pool.lane, None);
            let launcher_name = config
                .lane_launcher_name(&pool.lane)
                .unwrap_or(pool.lane.as_str());
            let model_str = format_model(launcher_name, &model.provider, &model.name);
            for i in 0..pool.min {
                let name = name_iter
                    .next()
                    .unwrap_or_else(|| format!("{}-research-{}", pool.id, i + 1));
                research_cli_map.insert(name.clone(), agent_to_adapter(&pool.lane, &config));
                research_agent_type_map.insert(name.clone(), pool.id.clone());
                research_model_map.insert(name.clone(), model_str.clone());
                let mut research_env = launcher_env_pairs(&pool.lane);
                research_env.push(("BREHON_RESEARCH_POOL_ID".to_string(), pool.id.clone()));
                research_env.push(("BREHON_RESEARCH_POOL_LANE".to_string(), pool.lane.clone()));
                research_env.push(("BREHON_RESEARCH_ROLE".to_string(), pool.role.clone()));
                if let Some(policy) = combine_startup_policy(
                    research_project_policy.as_deref(),
                    config.lane_system_prompt(&pool.lane, pool.instruction_profile.as_deref()),
                ) {
                    research_env.push(("BREHON_ROLE_SYSTEM_PROMPT".to_string(), policy.clone()));
                    research_startup_policy_map.insert(name.clone(), policy);
                }
                research_env_map.insert(name.clone(), research_env);
                if let Some(effort) = reasoning_effort {
                    research_reasoning_effort_map.insert(name.clone(), effort.to_string());
                }
                research_names.push(name);
            }
        }
    }

    let reviewer_panels = build_reviewer_panels(&config, &pool_reviewer_names);
    let planned_review_panel_seats =
        build_planned_review_panel_seats(&config, &pool_reviewer_names);
    let (reviewer_panel_map, reviewer_panel_tab_map) =
        reviewer_panel_terminal_host_maps(&reviewer_panels);

    let supervisor_adapter = agent_to_adapter(&config.roles.supervisor.name, &config);

    // ── Agent Teams setup for Claude Code agents ────────────────────────
    // Claude Code uses native Agent Teams for prompt delivery and inter-agent
    // messaging. Other ACP-backed CLIs (OpenCode, Gemini, Codex) receive a
    // delayed startup prompt after the gateway session is online.
    let is_claude = |a: &brehon_mux::AgentAdapter| a.as_builtin() == Some(SupervisorCli::Claude);

    let has_claude_agents = is_claude(&supervisor_adapter)
        || worker_cli_map.values().any(&is_claude)
        || reviewer_cli_map.values().any(&is_claude)
        || advisor_cli_map.values().any(&is_claude)
        || research_cli_map.values().any(&is_claude);

    let mut teams_configs = std::collections::HashMap::new();
    let teams_lead_session_id = uuid::Uuid::new_v4().to_string();

    if has_claude_agents {
        let teams_mgr = brehon_mux::teams::TeamsManager::new(&session_name);

        // Claude Code workers
        let mut claude_idx = 0usize;
        for name in &worker_names {
            if worker_cli_map.get(name).map(&is_claude).unwrap_or(false) {
                teams_configs.insert(
                    name.clone(),
                    teams_mgr.spawn_config_for(
                        name,
                        None,
                        "general-purpose",
                        brehon_mux::teams::TeamsManager::color_for_index(claude_idx),
                        Some(&teams_lead_session_id),
                    ),
                );
                claude_idx += 1;
            }
        }

        // Supervisor (if Claude)
        if is_claude(&supervisor_adapter) {
            teams_configs.insert(
                config.roles.supervisor.name.clone(),
                teams_mgr.spawn_config_for(
                    &config.roles.supervisor.name,
                    None,
                    "team-lead",
                    brehon_mux::teams::InboxMessageColor::Green,
                    None,
                ),
            );
        }

        // Claude Code reviewers
        for name in &reviewer_names {
            if reviewer_cli_map.get(name).map(&is_claude).unwrap_or(false) {
                teams_configs.insert(
                    name.clone(),
                    teams_mgr.spawn_config_for(
                        name,
                        None,
                        "general-purpose",
                        brehon_mux::teams::TeamsManager::color_for_index(claude_idx),
                        Some(&teams_lead_session_id),
                    ),
                );
                claude_idx += 1;
            }
        }

        // Claude Code advisors
        for name in &advisor_names {
            if advisor_cli_map.get(name).map(&is_claude).unwrap_or(false) {
                teams_configs.insert(
                    name.clone(),
                    teams_mgr.spawn_config_for(
                        name,
                        None,
                        "general-purpose",
                        brehon_mux::teams::TeamsManager::color_for_index(claude_idx),
                        Some(&teams_lead_session_id),
                    ),
                );
                claude_idx += 1;
            }
        }

        // Claude Code research agents
        for name in &research_names {
            if research_cli_map.get(name).map(&is_claude).unwrap_or(false) {
                teams_configs.insert(
                    name.clone(),
                    teams_mgr.spawn_config_for(
                        name,
                        None,
                        "general-purpose",
                        brehon_mux::teams::TeamsManager::color_for_index(claude_idx),
                        Some(&teams_lead_session_id),
                    ),
                );
                claude_idx += 1;
            }
        }
    }

    // Get terminal size for initial pane dimensions
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));

    // Derive the default worker model from the first pool
    let default_worker_model = config.roles.workers.first().and_then(|p| {
        let model = config.lane_model(&p.lane, p.model.as_ref())?;
        let launcher = config
            .lane_launcher_name(&p.lane)
            .unwrap_or(p.lane.as_str());
        Some(format_model(launcher, &model.provider, &model.name))
    });
    let default_reviewer_model = config.roles.reviewers.first().and_then(|p| {
        let model = config.lane_model(&p.lane, p.model.as_ref())?;
        let launcher = config
            .lane_launcher_name(&p.lane)
            .unwrap_or(p.lane.as_str());
        Some(format_model(launcher, &model.provider, &model.name))
    });
    let default_advisor_model = config.advisors.pools.first().and_then(|p| {
        let model = config.lane_model(&p.lane, p.model.as_ref())?;
        let launcher = config
            .lane_launcher_name(&p.lane)
            .unwrap_or(p.lane.as_str());
        Some(format_model(launcher, &model.provider, &model.name))
    });
    let default_research_model = config.research.pools.first().and_then(|p| {
        let model = config.lane_model(&p.lane, None)?;
        let launcher = config
            .lane_launcher_name(&p.lane)
            .unwrap_or(p.lane.as_str());
        Some(format_model(launcher, &model.provider, &model.name))
    });

    splash.set_stage("Preparing worker worktrees");
    let worker_cwds = prepare_scoped_worktrees_with_progress(
        &cwd,
        &config,
        Some(&session_name),
        None,
        &worker_names,
        |message| splash.record(message),
    )
    .await?;
    splash.set_stage("Preparing supervisor worktree");
    let supervisor_cwds = match prepare_scoped_worktrees_with_progress(
        &cwd,
        &config,
        Some(&session_name),
        Some("supervisor"),
        std::slice::from_ref(&config.roles.supervisor.name),
        |message| splash.record(message),
    )
    .await
    {
        Ok(cwds) => cwds,
        Err(err) => {
            if config.orchestration.auto_cleanup_worktrees {
                cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
            }
            return Err(err);
        }
    };
    splash.set_stage("Preparing reviewer worktrees");
    let reviewer_cwds = match prepare_scoped_worktrees_with_progress(
        &cwd,
        &config,
        Some(&session_name),
        Some("reviewer"),
        &reviewer_names,
        |message| splash.record(message),
    )
    .await
    {
        Ok(cwds) => cwds,
        Err(err) => {
            if config.orchestration.auto_cleanup_worktrees {
                cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
                cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
            }
            return Err(err);
        }
    };
    splash.set_stage("Preparing advisor worktrees");
    let advisor_cwds = match prepare_scoped_worktrees_with_progress(
        &cwd,
        &config,
        Some(&session_name),
        Some("advisor"),
        &advisor_names,
        |message| splash.record(message),
    )
    .await
    {
        Ok(cwds) => cwds,
        Err(err) => {
            if config.orchestration.auto_cleanup_worktrees {
                cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
                cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
                cleanup_scoped_worktrees(&cwd, &reviewer_cwds).await;
            }
            return Err(err);
        }
    };
    splash.set_stage("Preparing research worktrees");
    let research_cwds = match prepare_scoped_worktrees_with_progress(
        &cwd,
        &config,
        Some(&session_name),
        Some("research"),
        &research_names,
        |message| splash.record(message),
    )
    .await
    {
        Ok(cwds) => cwds,
        Err(err) => {
            if config.orchestration.auto_cleanup_worktrees {
                cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
                cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
                cleanup_scoped_worktrees(&cwd, &reviewer_cwds).await;
                cleanup_scoped_worktrees(&cwd, &advisor_cwds).await;
            }
            return Err(err);
        }
    };
    let supervisor_cwd = supervisor_cwds.get(&config.roles.supervisor.name).cloned();
    let mut supervisor_env = launcher_env_pairs(&config.roles.supervisor.name);
    if let Some(policy) = config.project_prompt_for_role_name("supervisor") {
        supervisor_env.push(("BREHON_ROLE_SYSTEM_PROMPT".to_string(), policy));
    }
    let runtime_policy_gate: Arc<dyn PolicyGate> =
        Arc::new(brehon_policy::BasicPolicyGate::default());
    let RuntimeTerminalHostWiring {
        runtime_terminal_host,
        terminal_host_event_forwarder,
        runtime_command_port,
        runtime_command_rx,
        terminal_host_command_port,
        mut terminal_host_status,
    } = build_runtime_terminal_host_wiring(&config.runtime.terminal_host, &session_name)?;
    let host_owns_agent_panes =
        terminal_host_status.pane_ownership == brehon_types::RuntimeTerminalHostPaneOwnership::Host;
    let terminal_host_absolute_resize = terminal_host_status
        .capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.absolute_resize);
    if let Some(host) = runtime_terminal_host.as_ref() {
        splash.record(format!(
            "Using experimental {:?} terminal host; daemon commands routed to {:?}",
            host.adapter().capabilities().source,
            terminal_host_status.command_routing
        ));
    }
    let runtime_audit_log_path = cwd
        .join(".brehon")
        .join("runtime")
        .join("audit")
        .join(format!("{session_name}.jsonl"));
    let runtime_approval_store_path = cwd
        .join(".brehon")
        .join("runtime")
        .join("daemon")
        .join("approvals.json");
    let runtime_event_forwarder = Arc::new(LateBindingRuntimeEventSink::default());
    let mux_runtime_event_sink: Arc<dyn RuntimeEventSink> = runtime_event_forwarder.clone();
    let mut terminal_host_preview_pane = None;

    let mux_config = MuxConfig {
        cwd: cwd.clone(),
        session_name: Some(session_name.clone()),
        brehon_root: Some(cwd.join(".brehon")),
        worktree_isolation: config.orchestration.worktree_isolation,
        pane_materialization: if host_owns_agent_panes {
            brehon_mux::AgentPaneMaterialization::PlanOnly
        } else {
            brehon_mux::AgentPaneMaterialization::Spawn
        },
        worker_cwds: worker_cwds.clone(),
        supervisor_cwd,
        reviewer_cwds: reviewer_cwds.clone(),
        advisor_cwds: advisor_cwds.clone(),
        research_cwds: research_cwds.clone(),
        workers: worker_count,
        worker_names: worker_names.clone(),
        supervisor_name: config.roles.supervisor.name.clone(),
        supervisor_cli: supervisor_adapter.clone(),
        supervisor_model: resolved_supervisor_model(&config, format_model),
        supervisor_reasoning_effort: resolved_supervisor_reasoning_effort(&config),
        worker_cli: agent_to_adapter(
            config
                .roles
                .workers
                .first()
                .map(|w| w.lane.as_str())
                .unwrap_or("claude-code"),
            &config,
        ),
        worker_cli_map: worker_cli_map.clone(),
        worker_agent_type_map,
        worker_env_map,
        worker_model_map,
        worker_reasoning_effort_map,
        worker_model: default_worker_model,
        reviewer_names: reviewer_names.clone(),
        reviewer_cli: agent_to_adapter(
            config
                .roles
                .reviewers
                .first()
                .map(|r| r.lane.as_str())
                .unwrap_or("codex"),
            &config,
        ),
        reviewer_cli_map: reviewer_cli_map.clone(),
        reviewer_agent_type_map,
        reviewer_env_map,
        reviewer_model_map,
        reviewer_reasoning_effort_map,
        reviewer_model: default_reviewer_model,
        reviewer_panel_map,
        reviewer_panel_tab_map,
        advisor_names: advisor_names.clone(),
        advisor_cli: agent_to_adapter(
            config
                .advisors
                .pools
                .first()
                .map(|p| p.lane.as_str())
                .unwrap_or("codex"),
            &config,
        ),
        advisor_cli_map: advisor_cli_map.clone(),
        advisor_agent_type_map: advisor_agent_type_map.clone(),
        advisor_env_map,
        advisor_model_map,
        advisor_reasoning_effort_map,
        advisor_model: default_advisor_model,
        research_names: research_names.clone(),
        research_cli: agent_to_adapter(
            config
                .research
                .pools
                .first()
                .map(|p| p.lane.as_str())
                .unwrap_or("codex"),
            &config,
        ),
        research_cli_map: research_cli_map.clone(),
        research_agent_type_map: research_agent_type_map.clone(),
        research_env_map,
        research_model_map,
        research_reasoning_effort_map,
        research_model: default_research_model,
        supervisor_agent_type: Some(config.roles.supervisor.name.clone()),
        supervisor_env,
        teams_configs,
        direct_tool_bridge_factory: Some(BrehonDirectToolBridgeFactory::new()),
        runtime_event_sink: Some(mux_runtime_event_sink),
        policy_gate: Some(runtime_policy_gate.clone()),
        launch_policy: Some(brehon_pty::LaunchPolicy::from_security_config(
            &config.security,
        )),
        include_director: false, // No director pane for now
        rows,
        cols,
        sandbox_profile: Some(config.security.sandbox_profile),
        ..Default::default()
    };

    splash.set_stage("Preparing runtime session");
    match prepare_runtime_session_state(&cwd, &session_name) {
        Ok(stale_prompt_count) => {
            if stale_prompt_count > 0 {
                splash.record(format!(
                    "Archived {stale_prompt_count} stale prompt queue file(s) from previous sessions"
                ));
            }
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "Failed to prepare runtime session state: {err}"
            ));
        }
    }
    if config.advisors.enabled {
        splash.record("Seeding advisor rooms".to_string());
        if let Err(err) = seed_configured_advisor_rooms(
            &cwd.join(".brehon"),
            &config,
            &advisor_names,
            &advisor_agent_type_map,
        ) {
            if config.orchestration.auto_cleanup_worktrees {
                cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
                cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
                cleanup_scoped_worktrees(&cwd, &reviewer_cwds).await;
                cleanup_scoped_worktrees(&cwd, &advisor_cwds).await;
                cleanup_scoped_worktrees(&cwd, &research_cwds).await;
            }
            return Err(anyhow::anyhow!("Failed to seed advisor rooms: {err}"));
        }
    }

    tracing::info!(
        workers = mux_config.workers,
        supervisor = %mux_config.supervisor_name,
        reviewers = mux_config.reviewer_names.len(),
        advisors = mux_config.advisor_names.len(),
        researchers = mux_config.research_names.len(),
        "Creating mux"
    );
    splash.set_stage("Creating panes");
    splash.record("Creating mux".to_string());

    let mut mux = match Mux::factory(mux_config) {
        Ok(mux) => mux,
        Err(err) => {
            if config.orchestration.auto_cleanup_worktrees {
                cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
                cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
                cleanup_scoped_worktrees(&cwd, &reviewer_cwds).await;
                cleanup_scoped_worktrees(&cwd, &advisor_cwds).await;
                cleanup_scoped_worktrees(&cwd, &research_cwds).await;
            }
            return Err(anyhow::anyhow!("Mux creation failed: {}", err));
        }
    };

    let agent_factory_plan = mux.terminal_host_agent_factory_plan(&session_name);
    let agent_factory_owner = if host_owns_agent_panes && agent_factory_plan.ready() {
        brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::TerminalHost
    } else {
        brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::Mux
    };
    terminal_host_status = terminal_host_status_with_agent_factory_plan_and_owner(
        terminal_host_status,
        &agent_factory_plan,
        agent_factory_owner,
    );
    if host_owns_agent_panes && !agent_factory_plan.ready() {
        if config.orchestration.auto_cleanup_worktrees {
            cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
            cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
            cleanup_scoped_worktrees(&cwd, &reviewer_cwds).await;
            cleanup_scoped_worktrees(&cwd, &advisor_cwds).await;
            cleanup_scoped_worktrees(&cwd, &research_cwds).await;
        }
        let blockers = agent_factory_plan
            .blocked_panes
            .iter()
            .map(|pane| format!("{} '{}': {}", pane.kind, pane.pane_id, pane.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(anyhow::anyhow!(
            "runtime.terminal_host.pane_ownership=host requires all worker/reviewer/advisor/supervisor panes to be terminal-host PTY eligible; blocked panes: {}",
            blockers
        ));
    } else if !agent_factory_plan.blocked_panes.is_empty() {
        splash.record(format!(
            "Terminal-host agent factory remains mux-owned: {} of {} pane(s) are not host-eligible",
            agent_factory_plan.blocked_panes.len(),
            agent_factory_plan.total_panes
        ));
    } else if host_owns_agent_panes {
        splash.record(format!(
            "Terminal-host agent factory will launch {} pane(s)",
            agent_factory_plan.launch_specs.len()
        ));
    }
    let runtime_daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
        policy_gate: Some(runtime_policy_gate),
        command_port: Some(runtime_command_port),
        audit_log_path: Some(runtime_audit_log_path.clone()),
        approval_store_path: Some(runtime_approval_store_path.clone()),
        approval_store_session_id: Some(session_name.clone()),
        terminal_host: Some(terminal_host_status),
        ..Default::default()
    });
    let approval_store_report = runtime_daemon.load_persisted_approvals().await?;
    if approval_store_report.loaded > 0 {
        splash.record(format!(
            "Restored {} pending runtime approval(s)",
            approval_store_report.loaded
        ));
    }
    if approval_store_report.ignored_stale > 0 {
        splash.record(format!(
            "Cleared {} stale runtime approval(s)",
            approval_store_report.ignored_stale
        ));
    }
    let runtime_event_sink: Arc<dyn RuntimeEventSink> = Arc::new(runtime_daemon.clone());
    runtime_event_forwarder.bind(runtime_event_sink.clone());
    terminal_host_event_forwarder.bind(runtime_event_sink.clone());
    let runtime_command_router: Arc<dyn RuntimeCommandRouter> = Arc::new(runtime_daemon.clone());
    let (runtime_event_ui_tx, runtime_event_ui_rx) = std::sync::mpsc::sync_channel(2048);
    let mut runtime_event_subscription = runtime_daemon.subscribe();
    let runtime_event_ui_handle = tokio::spawn(async move {
        loop {
            match runtime_event_subscription.next_event().await {
                Ok(Some(event)) => match runtime_event_ui_tx.try_send(event) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        tracing::warn!("Runtime event UI queue full; dropping event");
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                },
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "Runtime event UI subscription lagged or failed"
                    );
                }
            }
        }
    });
    let mut terminal_host_observation_shutdown: Option<tokio::sync::watch::Sender<bool>> = None;
    let mut terminal_host_observation_handle: Option<
        tokio::task::JoinHandle<Result<(), brehon_ports::PortError>>,
    > = None;

    if host_owns_agent_panes {
        splash.set_stage("Launching terminal-host agent panes");
        match launch_terminal_host_agent_factory_plan_with_progress(
            &runtime_daemon,
            &session_name,
            &agent_factory_plan,
            |index, total, launch| {
                let title = launch
                    .spec
                    .title
                    .as_deref()
                    .unwrap_or(launch.spec.pane_id.as_str());
                splash.record(format!(
                    "Launching terminal-host pane {index}/{total}: {title}"
                ));
            },
        )
        .await
        {
            Ok(report) => {
                splash.record(format!(
                    "Launched {} terminal-host agent pane(s)",
                    report.launched
                ));
                let registry = runtime_daemon.pane_registry_snapshot().await;
                for launch in &agent_factory_plan.launch_specs {
                    let pane_id = launch.spec.pane_id.as_str();
                    if let Some(entry) = registry
                        .panes
                        .iter()
                        .find(|entry| entry.session_id == session_name && entry.pane_id == pane_id)
                    {
                        if let Err(err) =
                            mux.sync_terminal_host_pane_generation(pane_id, entry.generation)
                        {
                            tracing::warn!(
                                pane = %pane_id,
                                generation = entry.generation,
                                error = %err,
                                "Failed to sync terminal-host pane generation into mux"
                            );
                        }
                    }
                }
            }
            Err(err) => {
                if let Some(shutdown_tx) = terminal_host_observation_shutdown.take() {
                    let _ = shutdown_tx.send(true);
                }
                if let Some(handle) = terminal_host_observation_handle.take() {
                    handle.abort();
                    let _ = handle.await;
                }
                runtime_event_ui_handle.abort();
                let _ = runtime_event_ui_handle.await;
                runtime_daemon.shutdown().await;
                if let Some(host) = runtime_terminal_host.as_ref() {
                    if let Err(cleanup_err) = host.shutdown().await {
                        tracing::warn!(
                            error = %cleanup_err,
                            "Terminal host cleanup failed after agent factory launch error"
                        );
                    }
                }
                if config.orchestration.auto_cleanup_worktrees {
                    cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
                    cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
                    cleanup_scoped_worktrees(&cwd, &reviewer_cwds).await;
                    cleanup_scoped_worktrees(&cwd, &advisor_cwds).await;
                    cleanup_scoped_worktrees(&cwd, &research_cwds).await;
                }
                return Err(anyhow::anyhow!(
                    "Failed to launch terminal-host agent panes: {err}"
                ));
            }
        }
    }

    if !host_owns_agent_panes
        && runtime_terminal_host_preview_enabled(&config.runtime.terminal_host)
    {
        terminal_host_preview_pane = match terminal_host_command_port.as_ref() {
            Some(command_port) => {
                match spawn_terminal_host_preview_pane(command_port, &session_name, &cwd).await {
                    Ok(result) if result.status == brehon_types::RuntimeCommandStatus::Applied => {
                        let registry = runtime_daemon.pane_registry_snapshot().await;
                        let pane =
                            terminal_host_preview_pane_from_registry(&registry, &session_name);
                        if pane.is_some() {
                            splash.record("Started experimental terminal-host preview pane");
                        } else {
                            splash.record(
                                "Started experimental terminal-host preview pane, but it was not found in the daemon registry for shutdown tracking",
                            );
                        }
                        pane
                    }
                    Ok(result) => {
                        splash.record(format!(
                            "Terminal-host preview pane not started: {}",
                            result
                                .message
                                .unwrap_or_else(|| format!("{:?}", result.status))
                        ));
                        None
                    }
                    Err(err) => {
                        splash.record(format!("Terminal-host preview pane failed: {err}"));
                        None
                    }
                }
            }
            None => {
                splash.record(
                    "terminal-host preview ignored because runtime.terminal_host.kind is embedded"
                        .to_string(),
                );
                None
            }
        };
    }
    if let Some(observer) = runtime_terminal_host
        .as_ref()
        .and_then(|host| host.observer())
    {
        let pump =
            brehon_host::TerminalHostObservationPump::new(observer, runtime_event_sink.clone());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        terminal_host_observation_shutdown = Some(shutdown_tx);
        terminal_host_observation_handle = Some(tokio::spawn(async move {
            pump.run_until_shutdown(shutdown_rx).await
        }));
    }
    let runtime_sidecar = brehon_daemon::RuntimeSidecar::start_with_enabled_workflows(
        runtime_daemon.clone(),
        config.runtime.enabled_workflows.clone(),
    );
    let runtime_sidecar_status = runtime_sidecar.status_handle();
    let runtime_daemon_status_path = cwd
        .join(".brehon")
        .join("runtime")
        .join("daemon")
        .join("current.json");
    let runtime_daemon_heartbeat = brehon_daemon::RuntimeDaemonHeartbeat::start(
        runtime_daemon_status_path.clone(),
        runtime_daemon.clone(),
        Some(runtime_sidecar_status.clone()),
        std::time::Duration::from_secs(5),
    );
    let runtime_command_inbox_path = cwd
        .join(".brehon")
        .join("runtime")
        .join("daemon")
        .join("commands");
    let runtime_command_inbox = brehon_daemon::RuntimeDaemonCommandInbox::start(
        runtime_command_inbox_path,
        runtime_daemon.clone(),
        std::time::Duration::from_secs(1),
    );

    splash.set_stage("Recovering runtime state");
    splash.record("Recovering orphaned worker assignments".to_string());
    match reconcile_orphaned_worker_assignments_for_run(&cwd.join(".brehon"), &worker_names) {
        Ok(repaired) => {
            for message in repaired {
                tracing::info!(message = %message, "Recovered orphaned worker assignment during startup");
                splash.record(message);
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "Failed to reconcile orphaned worker assignments during startup");
            splash.record(format!("Worker reconciliation warning: {err}"));
        }
    }

    splash.record("Reconciling persisted review runtime".to_string());
    if let Err(err) = reconcile_review_runtime_for_run(
        &cwd.join(".brehon"),
        &planned_review_panel_seats,
        &config.roles.supervisor.name,
        &config,
    ) {
        tracing::warn!(error = %err, "Failed to reconcile persisted review runtime during startup");
        splash.record(format!("Review runtime warning: {err}"));
    }

    splash.record("Finalizing worker assignment reconciliation".to_string());
    match reconcile_orphaned_worker_assignments_for_run(&cwd.join(".brehon"), &worker_names) {
        Ok(repaired) => {
            for message in repaired {
                tracing::info!(message = %message, "Reconciled worker assignments after review recovery during startup");
                splash.record(message);
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "Failed post-review worker reconciliation during startup");
            splash.record(format!("Post-review reconciliation warning: {err}"));
        }
    }

    // ── Initialize Teams file structure and queue startup prompts ────────
    if has_claude_agents {
        splash.set_stage("Configuring Teams");
        let teams_mgr = brehon_mux::teams::TeamsManager::new(&session_name);

        // All agent names go into config.json so Claude agents can see the
        // full team roster (including non-Claude members like OpenCode supervisor).
        let all_member_names: Vec<String> = worker_names
            .iter()
            .chain(reviewer_names.iter())
            .chain(advisor_names.iter())
            .chain(research_names.iter())
            .cloned()
            .collect();
        let team_member_cwds = build_team_member_cwds(
            &worker_cwds,
            &reviewer_cwds,
            &advisor_cwds,
            &research_cwds,
            &config.roles.supervisor.name,
            &supervisor_cwds,
        );

        if let Err(e) = teams_mgr.init_team_config(
            &config.roles.supervisor.name,
            &all_member_names,
            &cwd,
            &team_member_cwds,
            &teams_lead_session_id,
        ) {
            tracing::warn!("Failed to initialize Teams config: {:?}", e);
            splash.record(format!("Teams configuration warning: {e}"));
        } else {
            splash.record(format!(
                "Initialized Teams config for {} members",
                all_member_names.len() + 1
            ));
        }

        mux.set_teams(teams_mgr);
    }

    // Queue startup prompts for agents that need an explicit first turn.
    // Claude Code receives these via Teams inbox; gateway-backed agents receive
    // them after their session is online. PTY supervisors with embedded
    // startup prompts must not also be queued here.
    let needs_post_spawn_prompt =
        |adapter: &brehon_mux::AgentAdapter| -> bool { adapter.needs_post_spawn_prompt() };
    let adapter_uses_gateway = |adapter: &brehon_mux::AgentAdapter| -> bool {
        matches!(
            adapter.capabilities().preferred_control_plane,
            HarnessControlPlane::Acp
                | HarnessControlPlane::AcpSidecar
                | HarnessControlPlane::OpenAiCompatible
        )
    };
    let skip_preview_gateway_startup = |adapter: &brehon_mux::AgentAdapter| -> bool {
        !host_owns_agent_panes && !eager_gateway_bootstrap && adapter_uses_gateway(adapter)
    };

    // Worker startup prompts
    splash.set_stage("Queueing startup prompts");
    let mut terminal_host_startup_prompt_slot = 0u64;
    let mut terminal_host_startup_prompts = Vec::new();
    for name in &worker_names {
        let adapter = worker_cli_map
            .get(name)
            .unwrap_or(&AgentAdapter::BuiltIn(SupervisorCli::Claude));
        if needs_post_spawn_prompt(adapter) {
            if skip_preview_gateway_startup(adapter) {
                splash.record(format!(
                    "Skipped gateway startup prompt for worker {name} in terminal-host preview mode"
                ));
                continue;
            }
            let caps = adapter.capabilities();
            let agent_cmd = format!("{}agent", caps.tool_prefix);
            let task_cmd = format!("{}task", caps.tool_prefix);
            let prompt = brehon_pty::build_worker_startup_prompt(
                name,
                &config.roles.supervisor.name,
                &agent_cmd,
                &task_cmd,
                worker_startup_policy_map.get(name).map(String::as_str),
            );
            if host_owns_agent_panes {
                let delay = terminal_host_startup_prompt_delay(terminal_host_startup_prompt_slot);
                terminal_host_startup_prompt_slot =
                    terminal_host_startup_prompt_slot.saturating_add(1);
                terminal_host_startup_prompts.push(TerminalHostStartupPrompt {
                    target: name.clone(),
                    prompt,
                    delay,
                });
                splash.record(format!(
                    "Queued terminal-host startup prompt for worker {name}"
                ));
            } else {
                mux.queue_startup_prompt(name, prompt);
                splash.record(format!("Queued startup prompt for worker {name}"));
            }
        }
    }

    // Supervisor startup prompt
    let supervisor_needs_post_spawn_prompt = mux
        .get(&config.roles.supervisor.name)
        .map(|pane| needs_post_spawn_prompt(pane.cli_type()))
        .unwrap_or_else(|| needs_post_spawn_prompt(&supervisor_adapter));
    if supervisor_needs_post_spawn_prompt {
        if skip_preview_gateway_startup(&supervisor_adapter) {
            splash.record(format!(
                "Skipped gateway startup prompt for supervisor {} in terminal-host preview mode",
                config.roles.supervisor.name
            ));
        } else {
            let supervisor_project_policy = config.project_prompt_for_role_name("supervisor");
            let caps = supervisor_adapter.capabilities();
            let agent_cmd = format!("{}agent", caps.tool_prefix);
            let task_cmd = format!("{}task", caps.tool_prefix);
            let prompt = brehon_pty::build_supervisor_startup_prompt(
                &config.roles.supervisor.name,
                &agent_cmd,
                &task_cmd,
                supervisor_project_policy.as_deref(),
            );
            if host_owns_agent_panes {
                let delay = terminal_host_startup_prompt_delay(terminal_host_startup_prompt_slot);
                terminal_host_startup_prompt_slot =
                    terminal_host_startup_prompt_slot.saturating_add(1);
                terminal_host_startup_prompts.push(TerminalHostStartupPrompt {
                    target: config.roles.supervisor.name.clone(),
                    prompt,
                    delay,
                });
                splash.record(format!(
                    "Queued terminal-host startup prompt for supervisor {}",
                    config.roles.supervisor.name
                ));
            } else {
                mux.queue_startup_prompt(&config.roles.supervisor.name, prompt);
                splash.record(format!(
                    "Queued startup prompt for supervisor {}",
                    config.roles.supervisor.name
                ));
            }
        }
    }

    // Reviewer startup prompts
    for name in &reviewer_names {
        let adapter = reviewer_cli_map
            .get(name)
            .unwrap_or(&AgentAdapter::BuiltIn(SupervisorCli::Codex));
        if needs_post_spawn_prompt(adapter) {
            if skip_preview_gateway_startup(adapter) {
                splash.record(format!(
                    "Skipped gateway startup prompt for reviewer {name} in terminal-host preview mode"
                ));
                continue;
            }
            let caps = adapter.capabilities();
            let agent_cmd = format!("{}agent", caps.tool_prefix);
            let verification_cmd = format!("{}verification", caps.tool_prefix);
            let prompt = brehon_pty::build_reviewer_startup_prompt(
                name,
                &agent_cmd,
                &verification_cmd,
                reviewer_startup_policy_map.get(name).map(String::as_str),
            );
            if host_owns_agent_panes {
                let delay = terminal_host_startup_prompt_delay(terminal_host_startup_prompt_slot);
                terminal_host_startup_prompt_slot =
                    terminal_host_startup_prompt_slot.saturating_add(1);
                terminal_host_startup_prompts.push(TerminalHostStartupPrompt {
                    target: name.clone(),
                    prompt,
                    delay,
                });
                splash.record(format!(
                    "Queued terminal-host startup prompt for reviewer {name}"
                ));
            } else {
                mux.queue_startup_prompt(name, prompt);
                splash.record(format!("Queued startup prompt for reviewer {name}"));
            }
        }
    }

    // Advisor startup prompts
    for name in &advisor_names {
        let adapter = advisor_cli_map
            .get(name)
            .unwrap_or(&AgentAdapter::BuiltIn(SupervisorCli::Codex));
        if needs_post_spawn_prompt(adapter) {
            if skip_preview_gateway_startup(adapter) {
                splash.record(format!(
                    "Skipped gateway startup prompt for advisor {name} in terminal-host preview mode"
                ));
                continue;
            }
            let caps = adapter.capabilities();
            let agent_cmd = format!("{}agent", caps.tool_prefix);
            let advisor_cmd = format!("{}advisor", caps.tool_prefix);
            let prompt = brehon_pty::build_advisor_startup_prompt(
                name,
                &agent_cmd,
                &advisor_cmd,
                advisor_startup_policy_map.get(name).map(String::as_str),
            );
            if host_owns_agent_panes {
                let delay = terminal_host_startup_prompt_delay(terminal_host_startup_prompt_slot);
                terminal_host_startup_prompt_slot =
                    terminal_host_startup_prompt_slot.saturating_add(1);
                terminal_host_startup_prompts.push(TerminalHostStartupPrompt {
                    target: name.clone(),
                    prompt,
                    delay,
                });
                splash.record(format!(
                    "Queued terminal-host startup prompt for advisor {name}"
                ));
            } else {
                mux.queue_startup_prompt(name, prompt);
                splash.record(format!("Queued startup prompt for advisor {name}"));
            }
        }
    }

    // Research startup prompts
    for name in &research_names {
        let adapter = research_cli_map
            .get(name)
            .unwrap_or(&AgentAdapter::BuiltIn(SupervisorCli::Codex));
        if needs_post_spawn_prompt(adapter) {
            if skip_preview_gateway_startup(adapter) {
                splash.record(format!(
                    "Skipped gateway startup prompt for research agent {name} in terminal-host preview mode"
                ));
                continue;
            }
            let caps = adapter.capabilities();
            let agent_cmd = format!("{}agent", caps.tool_prefix);
            let research_cmd = format!("{}research", caps.tool_prefix);
            let prompt = brehon_pty::build_research_startup_prompt(
                name,
                &agent_cmd,
                &research_cmd,
                research_agent_type_map.get(name).map(String::as_str),
                research_startup_policy_map.get(name).map(String::as_str),
            );
            if host_owns_agent_panes {
                let delay = terminal_host_startup_prompt_delay(terminal_host_startup_prompt_slot);
                terminal_host_startup_prompt_slot =
                    terminal_host_startup_prompt_slot.saturating_add(1);
                terminal_host_startup_prompts.push(TerminalHostStartupPrompt {
                    target: name.clone(),
                    prompt,
                    delay,
                });
                splash.record(format!(
                    "Queued terminal-host startup prompt for research agent {name}"
                ));
            } else {
                mux.queue_startup_prompt(name, prompt);
                splash.record(format!("Queued startup prompt for research agent {name}"));
            }
        }
    }

    // Start all ACP-backed agents immediately so CLIs that self-bootstrap via
    // startup flags or MCP instructions can register without waiting for a
    // later prompt delivery.
    if eager_gateway_bootstrap {
        splash.set_stage("Bootstrapping agent sessions");
        mux.bootstrap_gateway_panes_with_progress(|message| splash.record(message))
            .await;
    } else {
        splash.record(format!(
            "Skipped eager ACP gateway bootstrap for terminal-host preview mode; set {EXPERIMENTAL_TERMINAL_HOST_EAGER_GATEWAY_BOOTSTRAP_ENV}=1 to force full agent startup before TUI"
        ));
    }

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    crate::signals::setup_signal_handlers(shutdown_flag.clone())?;

    // Dashboard data shared between TUI and optional background refresh
    let dashboard_data = Arc::new(std::sync::Mutex::new(brehon_tui::DashboardData {
        brehon_root: Some(cwd.join(".brehon")),
        ..Default::default()
    }));

    let terminal_host_startup_prompt_handle =
        if host_owns_agent_panes && !terminal_host_startup_prompts.is_empty() {
            let router = runtime_command_router.clone();
            let prompt_session_name = session_name.clone();
            Some(tokio::spawn(dispatch_terminal_host_startup_prompts(
                router,
                prompt_session_name,
                terminal_host_startup_prompts,
            )))
        } else {
            None
        };

    // Grab a handle to the current tokio runtime so the TUI can call async Pane::write()
    let rt = tokio::runtime::Handle::current();
    splash.set_stage("Launching TUI");
    splash.record("Handing off to the main interface".to_string());
    splash.finish();

    let operator_handle = {
        let tui_shutdown = shutdown_flag.clone();
        let tui_dashboard = dashboard_data.clone();
        let tui_orchestration = config.orchestration.clone();
        let project_config_loader: brehon_tui::ProjectConfigLoader =
            std::sync::Arc::new(|brehon_root: &std::path::Path| {
                let project_root =
                    if brehon_root.file_name().and_then(|n| n.to_str()) == Some(".brehon") {
                        brehon_root.parent().unwrap_or(brehon_root)
                    } else {
                        brehon_root
                    };
                brehon_config::load_config(Some(project_root)).ok()
            });
        tokio::task::spawn_blocking(move || {
            if let Err(e) = brehon_tui::run_tui_with_panels_and_runtime_commands(
                tui_shutdown,
                mux,
                rt,
                &reviewer_panels,
                tui_dashboard,
                tui_orchestration,
                runtime_command_rx,
                Some(runtime_event_ui_rx),
                Some(runtime_command_router),
                host_owns_agent_panes,
                terminal_host_absolute_resize,
                project_config_loader,
            ) {
                tracing::error!("TUI error: {:?}", e);
            }
        })
    };

    let review_maintenance_shutdown = shutdown_flag.clone();
    let review_maintenance_dashboard = dashboard_data.clone();
    let review_maintenance_supervisor = config.roles.supervisor.name.clone();
    let review_maintenance_tool =
        brehon_mcp::tools::verification::VerificationTool::new().with_config(config.review.clone());
    let review_maintenance_handle = tokio::spawn(async move {
        let sweep_interval = std::time::Duration::from_secs(60);
        while !review_maintenance_shutdown.load(Ordering::SeqCst) {
            tokio::time::sleep(sweep_interval).await;
            // Register first so drain tracking cannot miss a sweep entering
            // execution if shutdown flips between checks.
            let _sweep_guard = brehon_types::drain::in_flight_guard("review-maintenance-sweep");
            if crate::signals::is_draining() {
                // Refuse new review maintenance work during drain, but
                // allow any in-progress sweep to complete.
                break;
            }
            let actions = review_maintenance_tool
                .sweep_collecting_reviews(&review_maintenance_supervisor)
                .await;
            for action in actions {
                tracing::warn!(action = %action.message(), "Background review maintenance action");
                push_runtime_dashboard_event(&review_maintenance_dashboard, action.message());
            }
        }
    });

    // Wait for the operator surface to exit.
    let _ = operator_handle.await;
    if let Some(handle) = terminal_host_startup_prompt_handle {
        handle.abort();
        let _ = handle.await;
    }
    runtime_event_ui_handle.abort();
    let _ = runtime_event_ui_handle.await;
    shutdown_flag.store(true, Ordering::SeqCst);
    brehon_types::drain::set_draining();

    // From here on, the user sees a cooked terminal post-TUI. Without
    // explicit feedback the 30s drain + worktree cleanup looks like a
    // hang. ShutdownProgress prints each phase to stderr.
    let progress = crate::ui::ShutdownProgress::start();

    // Drain in-flight work before cleanup.
    // Long-running operations (git commits, review submissions, MCP calls)
    // tracked via in_flight_guard() will complete if they finish within
    // the configured drain timeout; otherwise they are terminated.
    let drain_timeout =
        std::time::Duration::from_secs(config.orchestration.effective_drain_timeout_secs());
    let in_flight_at_start = brehon_types::drain::in_flight_count();
    if in_flight_at_start == 0 {
        progress.step("No in-flight work to drain");
    } else {
        progress.step(format!(
            "Draining {} in-flight task(s) (up to {}s)...",
            in_flight_at_start,
            drain_timeout.as_secs()
        ));
    }
    crate::signals::wait_for_shutdown(shutdown_flag.clone(), drain_timeout).await;
    let in_flight_remaining = brehon_types::drain::in_flight_count();
    if in_flight_at_start > 0 {
        if in_flight_remaining == 0 {
            progress.step(format!(
                "Drained {}/{} task(s) cleanly",
                in_flight_at_start, in_flight_at_start
            ));
        } else {
            progress.warn(format!(
                "Drain timeout: {} task(s) still running, will be terminated",
                in_flight_remaining
            ));
        }
    }

    review_maintenance_handle.abort();
    let _ = review_maintenance_handle.await;
    tokio::task::yield_now().await;
    if let (Some(command_port), Some(preview_pane)) = (
        terminal_host_command_port.as_ref(),
        terminal_host_preview_pane.as_ref(),
    ) {
        match close_terminal_host_preview_pane(command_port, &session_name, preview_pane).await {
            Ok(result) if result.status == brehon_types::RuntimeCommandStatus::Applied => {
                progress.step("Closed experimental terminal-host preview pane");
            }
            Ok(result) => {
                progress.warn(format!(
                    "Terminal-host preview pane close was not applied: {}",
                    result
                        .message
                        .unwrap_or_else(|| format!("{:?}", result.status))
                ));
            }
            Err(err) => {
                progress.warn(format!("Terminal-host preview pane close failed: {err}"));
                tracing::warn!(error = %err, "Terminal host preview pane close failed");
            }
        }
    }
    if let Some(shutdown_tx) = terminal_host_observation_shutdown.take() {
        let _ = shutdown_tx.send(true);
    }
    if let Some(handle) = terminal_host_observation_handle.take() {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                progress.warn(format!(
                    "Terminal host observation stopped with error: {err}"
                ));
                tracing::warn!(error = %err, "Terminal host observation stopped with error");
            }
            Err(err) => {
                progress.warn(format!("Terminal host observation task failed: {err}"));
                tracing::warn!(error = %err, "Terminal host observation task failed");
            }
        }
    }
    if let Some(host) = runtime_terminal_host.as_ref() {
        if let Err(err) = host.shutdown().await {
            progress.warn(format!("Terminal host cleanup failed: {err}"));
            tracing::warn!(error = %err, "Terminal host cleanup failed");
        }
    }
    runtime_command_inbox.shutdown().await;
    runtime_sidecar.shutdown().await;
    runtime_daemon.shutdown().await;
    runtime_daemon_heartbeat.shutdown().await;
    if let Err(err) = brehon_daemon::RuntimeDaemonHeartbeat::write_current_status(
        &runtime_daemon_status_path,
        &runtime_daemon,
        Some(&runtime_sidecar_status),
    )
    .await
    {
        progress.warn(format!("Failed to write runtime daemon heartbeat: {err}"));
        tracing::warn!(error = %err, "Failed to write final runtime daemon heartbeat");
    }

    match write_runtime_daemon_summary(
        &cwd.join(".brehon"),
        &session_name,
        &runtime_audit_log_path,
        &runtime_daemon,
    )
    .await
    {
        Ok(path) => progress.step(format!(
            "Wrote runtime daemon summary to {}",
            path.display()
        )),
        Err(err) => {
            progress.warn(format!("Failed to write runtime daemon summary: {err}"));
            tracing::warn!(error = %err, "Failed to write runtime daemon summary");
        }
    }

    progress.step("Stopping agent sessions...");
    match terminate_session_processes(Some(&cwd), &session_name, true) {
        Ok(survivors) if survivors.is_empty() => {}
        Ok(survivors) => {
            progress.warn(format!(
                "{} session-scoped process(es) survived shutdown",
                survivors.len()
            ));
            tracing::warn!(
                session_name = %session_name,
                survivors = ?survivors,
                "Session-scoped agent processes survived shutdown"
            );
        }
        Err(err) => {
            progress.warn(format!("Failed to reap session processes: {err}"));
            tracing::warn!(
                session_name = %session_name,
                error = %err,
                "Failed to reap session-scoped agent processes after shutdown"
            );
        }
    }

    progress.step("Stopping background Brehon processes...");
    match terminate_project_processes(Some(&cwd), &[], true) {
        Ok(survivors) if survivors.is_empty() => {}
        Ok(survivors) => {
            progress.warn(format!(
                "{} project-scoped process(es) survived shutdown",
                survivors.len()
            ));
            tracing::warn!(
                session_name = %session_name,
                survivors = ?survivors,
                "Project-scoped Brehon processes survived shutdown"
            );
        }
        Err(err) => {
            progress.warn(format!("Failed to reap project processes: {err}"));
            tracing::warn!(
                session_name = %session_name,
                error = %err,
                "Failed to reap project-scoped Brehon processes after shutdown"
            );
        }
    }

    // Clean up Teams directory (inbox files under ~/.claude/teams/{session})
    if has_claude_agents {
        progress.step("Cleaning up team inboxes...");
        let teams_mgr = brehon_mux::teams::TeamsManager::new(&session_name);
        teams_mgr.cleanup();
    }

    // Clean up session files
    let sessions_dir = cwd.join(".brehon").join("runtime").join("sessions");
    if sessions_dir.exists() {
        progress.step("Removing session files...");
        let _ = std::fs::remove_dir_all(&sessions_dir);
    }
    let current_session_path = cwd
        .join(".brehon")
        .join("runtime")
        .join("current-session.json");
    let _ = std::fs::remove_file(&current_session_path);

    if config.orchestration.auto_cleanup_worktrees {
        let total_worktrees = worker_cwds.len()
            + supervisor_cwds.len()
            + reviewer_cwds.len()
            + advisor_cwds.len()
            + research_cwds.len();
        if total_worktrees > 0 {
            progress.step(format!(
                "Cleaning up {} agent worktree(s)...",
                total_worktrees
            ));
        }
        cleanup_scoped_worktrees(&cwd, &worker_cwds).await;
        cleanup_scoped_worktrees(&cwd, &supervisor_cwds).await;
        cleanup_scoped_worktrees(&cwd, &reviewer_cwds).await;
        cleanup_scoped_worktrees(&cwd, &advisor_cwds).await;
        cleanup_scoped_worktrees(&cwd, &research_cwds).await;
    }

    if let Some(default_branch) = shared_root_default_branch.as_deref() {
        progress.step(format!(
            "Restoring shared root branch to '{}'...",
            default_branch
        ));
        restore_shared_root_branch(&cwd, default_branch)?;
    }

    progress.finish();
    Ok(())
}

async fn write_runtime_daemon_summary(
    brehon_root: &Path,
    session_name: &str,
    audit_log_path: &Path,
    daemon: &brehon_daemon::RuntimeDaemon,
) -> Result<PathBuf> {
    let dir = brehon_root.join("runtime").join("daemon");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session_name}.json"));
    let status = daemon.status(None).await;
    let payload = serde_json::json!({
        "session_name": session_name,
        "written_at": chrono::Utc::now().to_rfc3339(),
        "audit_log_path": audit_log_path.display().to_string(),
        "metrics": status.metrics,
        "registry": status.registry.clone(),
        "approvals": status.approvals.clone(),
        "terminal_host": status.terminal_host,
        "status": status,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_terminal_host_gate_stays_fail_closed_by_default() {
        assert!(runtime_terminal_host_supported(
            brehon_types::RuntimeTerminalHostKind::Embedded,
            false
        ));
        assert!(!runtime_terminal_host_supported(
            brehon_types::RuntimeTerminalHostKind::Web,
            true
        ));
        assert!(runtime_terminal_host_run_supported(
            brehon_types::RuntimeTerminalHostKind::Embedded,
            brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
            false
        ));
        assert!(!runtime_terminal_host_run_supported(
            brehon_types::RuntimeTerminalHostKind::Web,
            brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
            true
        ));
        assert!(!runtime_terminal_host_run_supported(
            brehon_types::RuntimeTerminalHostKind::Embedded,
            brehon_types::RuntimeTerminalHostPaneOwnership::Host,
            true
        ));
        assert!(runtime_terminal_host_run_supported(
            brehon_types::RuntimeTerminalHostKind::Headless,
            brehon_types::RuntimeTerminalHostPaneOwnership::Host,
            true
        ));
        assert!(!runtime_terminal_host_run_supported(
            brehon_types::RuntimeTerminalHostKind::Headless,
            brehon_types::RuntimeTerminalHostPaneOwnership::Host,
            false
        ));
    }

    #[test]
    fn startup_policy_combines_lane_prompt_and_project_policy() {
        let rendered = combine_startup_policy(Some("project rule"), Some("lane rule")).unwrap();

        assert!(rendered.contains("Lane system prompt:\nlane rule"));
        assert!(rendered.contains("project rule"));
        assert!(rendered.find("lane rule") < rendered.find("project rule"));
    }

    #[test]
    fn experimental_terminal_host_flag_accepts_explicit_truthy_values() {
        assert!(experimental_terminal_host_enabled_from_value(Some("1")));
        assert!(experimental_terminal_host_enabled_from_value(Some("true")));
        assert!(experimental_terminal_host_enabled_from_value(Some(" YES ")));
        assert!(experimental_terminal_host_enabled_from_value(Some("on")));
        assert!(!experimental_terminal_host_enabled_from_value(None));
        assert!(!experimental_terminal_host_enabled_from_value(Some("0")));
        assert!(!experimental_terminal_host_enabled_from_value(Some(
            "false"
        )));
    }

    #[test]
    fn terminal_host_preview_can_be_enabled_from_config_or_env() {
        assert!(!runtime_terminal_host_preview_enabled_from_parts(
            None, None
        ));
        assert!(!runtime_terminal_host_preview_enabled_from_parts(
            Some(false),
            None
        ));
        assert!(runtime_terminal_host_preview_enabled_from_parts(
            Some(true),
            None
        ));
        assert!(runtime_terminal_host_preview_enabled_from_parts(
            Some(false),
            Some("true")
        ));
        assert!(!runtime_terminal_host_preview_enabled_from_parts(
            None,
            Some("false")
        ));
    }

    #[test]
    fn preview_terminal_host_disables_eager_gateway_bootstrap_by_default() {
        let config = brehon_types::RuntimeTerminalHostConfig {
            kind: Some(brehon_types::RuntimeTerminalHostKind::Headless),
            preview_pane: Some(true),
            pane_ownership: Some(brehon_types::RuntimeTerminalHostPaneOwnership::Mux),
        };

        assert!(!eager_gateway_bootstrap_enabled_from_parts(
            &config, None, None
        ));
        assert!(eager_gateway_bootstrap_enabled_from_parts(
            &config,
            None,
            Some("1")
        ));
    }

    #[test]
    fn eager_gateway_bootstrap_stays_enabled_for_non_preview_or_host_owned_runs() {
        let embedded = brehon_types::RuntimeTerminalHostConfig::default();
        assert!(eager_gateway_bootstrap_enabled_from_parts(
            &embedded, None, None
        ));

        let host_owned = brehon_types::RuntimeTerminalHostConfig {
            kind: Some(brehon_types::RuntimeTerminalHostKind::Headless),
            preview_pane: Some(true),
            pane_ownership: Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host),
        };
        assert!(eager_gateway_bootstrap_enabled_from_parts(
            &host_owned,
            None,
            None
        ));
    }

    #[test]
    fn eager_gateway_bootstrap_can_be_disabled_explicitly() {
        let embedded = brehon_types::RuntimeTerminalHostConfig::default();

        assert!(!eager_gateway_bootstrap_enabled_from_parts(
            &embedded,
            None,
            Some("false")
        ));
    }

    #[derive(Debug, Default)]
    struct RecordingRuntimeEventSink {
        events: tokio::sync::Mutex<Vec<brehon_types::RuntimeEvent>>,
    }

    #[async_trait]
    impl RuntimeEventSink for RecordingRuntimeEventSink {
        async fn publish(
            &self,
            event: brehon_types::RuntimeEvent,
        ) -> std::result::Result<(), PortError> {
            self.events.lock().await.push(event);
            Ok(())
        }
    }

    #[tokio::test]
    async fn late_binding_event_sink_forwards_after_binding() {
        let sink = LateBindingRuntimeEventSink::default();
        let event = brehon_types::RuntimeEvent::new(
            brehon_types::RuntimeEventMeta::new(
                "session",
                "pane",
                1,
                brehon_types::RuntimeSource::Headless,
                1,
            ),
            brehon_types::RuntimeEventKind::PaneExited(brehon_types::PaneExitedEvent {
                exit_code: Some(0),
                reason: Some("done".to_string()),
            }),
        );

        sink.publish(event.clone())
            .await
            .expect("unbound sink should drop event");
        let target = Arc::new(RecordingRuntimeEventSink::default());
        sink.bind(target.clone());
        sink.publish(event.clone()).await.expect("publish bound");

        assert_eq!(target.events.lock().await.clone(), vec![event]);
    }

    #[test]
    fn embedded_run_wiring_keeps_mux_command_receiver() {
        let config = brehon_types::RuntimeTerminalHostConfig::default();
        let wiring =
            build_runtime_terminal_host_wiring(&config, "session").expect("embedded wiring");

        assert!(wiring.runtime_terminal_host.is_none());
        assert!(wiring.runtime_command_rx.is_some());
        assert!(wiring.terminal_host_command_port.is_none());
        assert_eq!(
            wiring.terminal_host_status.kind,
            brehon_types::RuntimeTerminalHostKind::Embedded
        );
        assert!(!wiring.terminal_host_status.experimental);
        assert!(!wiring.terminal_host_status.observation_running);
        assert_eq!(
            wiring.terminal_host_status.command_routing,
            brehon_daemon::RuntimeTerminalHostCommandRouting::Mux
        );
        assert_eq!(
            wiring.terminal_host_status.pane_ownership,
            brehon_types::RuntimeTerminalHostPaneOwnership::Mux
        );
        assert!(wiring.terminal_host_status.capabilities.is_none());
        assert_eq!(
            wiring.terminal_host_status.promotion_readiness.blockers,
            vec![
                "embedded host is the production default".to_string(),
                "daemon commands still route to mux".to_string(),
                "agent panes are still mux-owned".to_string(),
                "worker/reviewer/supervisor factory still mux-owned".to_string(),
                "terminal-host capabilities are missing".to_string(),
            ]
        );
        assert_eq!(wiring.terminal_host_status.session_name, None);
        assert_eq!(wiring.terminal_host_status.socket_name, None);
        assert_eq!(wiring.terminal_host_status.socket_dir, None);
        assert_eq!(wiring.terminal_host_status.binary_path, None);
    }

    #[test]
    fn terminal_host_status_includes_agent_factory_plan_blockers() {
        let config = brehon_types::RuntimeTerminalHostConfig {
            kind: Some(brehon_types::RuntimeTerminalHostKind::Headless),
            pane_ownership: Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host),
            ..Default::default()
        };
        let wiring = build_runtime_terminal_host_wiring(&config, "session")
            .expect("headless host-owned wiring");
        let plan = brehon_mux::TerminalHostAgentFactoryPlan {
            total_panes: 2,
            launch_specs: Vec::new(),
            blocked_panes: vec![brehon_mux::TerminalHostAgentFactoryBlockedPane {
                pane_id: "worker-1".to_string(),
                kind: "worker".to_string(),
                reason:
                    "gateway-backed codex_app_server_ws agent sessions are not terminal-host PTY panes"
                        .to_string(),
            }],
        };

        let status = terminal_host_status_with_agent_factory_plan_and_owner(
            wiring.terminal_host_status,
            &plan,
            brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::Mux,
        );

        assert!(!status.promotion_readiness.ready);
        assert!(status
            .promotion_readiness
            .blockers
            .contains(&"worker/reviewer/supervisor factory still mux-owned".to_string()));
        assert!(status
            .promotion_readiness
            .blockers
            .contains(&"1 of 2 mux-created pane is not terminal-host PTY eligible".to_string()));
        assert!(status.promotion_readiness.blockers.contains(
            &"worker 'worker-1' is not host-eligible: gateway-backed codex_app_server_ws agent sessions are not terminal-host PTY panes"
                .to_string()
        ));
    }

    #[tokio::test]
    async fn headless_host_owned_run_wiring_routes_daemon_commands_and_events() {
        let config = brehon_types::RuntimeTerminalHostConfig {
            kind: Some(brehon_types::RuntimeTerminalHostKind::Headless),
            pane_ownership: Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host),
            ..Default::default()
        };
        let RuntimeTerminalHostWiring {
            runtime_terminal_host,
            terminal_host_event_forwarder,
            runtime_command_port,
            runtime_command_rx,
            terminal_host_command_port,
            terminal_host_status,
        } = build_runtime_terminal_host_wiring(&config, "session").expect("headless wiring");

        assert!(runtime_command_rx.is_none());
        assert!(terminal_host_command_port.is_some());
        assert_eq!(
            terminal_host_status.kind,
            brehon_types::RuntimeTerminalHostKind::Headless
        );
        assert!(terminal_host_status.experimental);
        assert!(!terminal_host_status.observation_running);
        assert_eq!(
            terminal_host_status.command_routing,
            brehon_daemon::RuntimeTerminalHostCommandRouting::TerminalHost
        );
        assert_eq!(
            terminal_host_status.pane_ownership,
            brehon_types::RuntimeTerminalHostPaneOwnership::Host
        );
        assert!(
            terminal_host_status
                .capabilities
                .as_ref()
                .expect("headless capabilities")
                .absolute_resize
        );
        assert!(!terminal_host_status.promotion_readiness.ready);
        assert_eq!(
            terminal_host_status.promotion_readiness.blockers,
            vec!["worker/reviewer/supervisor factory still mux-owned".to_string()]
        );
        assert_eq!(
            terminal_host_status.session_name.as_deref(),
            Some("session")
        );

        let brehon_host::ConfiguredTerminalHost::Headless(headless_host) =
            runtime_terminal_host.as_ref().expect("headless host");
        let headless_host = headless_host.clone();
        let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(runtime_command_port),
            terminal_host: Some(terminal_host_status),
            ..brehon_daemon::RuntimeDaemonConfig::default()
        });
        let runtime_event_sink: Arc<dyn RuntimeEventSink> = Arc::new(daemon.clone());
        terminal_host_event_forwarder.bind(runtime_event_sink);

        let result = daemon
            .route_command(
                test_runtime_command(
                    "spawn",
                    None,
                    brehon_types::RuntimeCommandKind::SpawnPane {
                        kind: brehon_types::RuntimePaneKind::Worker,
                        pane_id: Some("pane".to_string()),
                        title: Some("worker".to_string()),
                        cwd: Some("/tmp".to_string()),
                        command: Vec::new(),
                        env: std::collections::BTreeMap::new(),
                        rows: Some(30),
                        cols: Some(100),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .expect("route spawn");
        assert_eq!(result.status, brehon_types::RuntimeCommandStatus::Applied);
        let registry = daemon.pane_registry_snapshot().await;
        assert_eq!(registry.panes.len(), 1);
        assert_eq!(
            registry.panes[0].state,
            brehon_types::RuntimePaneState::Ready
        );
        assert_eq!(
            registry.panes[0].kind,
            brehon_types::RuntimePaneKind::Worker
        );

        let result = daemon
            .route_command(
                test_runtime_command(
                    "input",
                    Some(1),
                    brehon_types::RuntimeCommandKind::SendTerminalInput {
                        bytes: b"hello".to_vec(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .expect("route input");
        assert_eq!(result.status, brehon_types::RuntimeCommandStatus::Applied);
        assert_eq!(
            headless_host
                .snapshot("session", "pane")
                .await
                .expect("headless pane")
                .input_bytes,
            b"hello"
        );

        let result = daemon
            .route_command(
                test_runtime_command(
                    "resize",
                    Some(1),
                    brehon_types::RuntimeCommandKind::ResizePane { rows: 22, cols: 90 },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .expect("route resize");
        assert_eq!(result.status, brehon_types::RuntimeCommandStatus::Applied);
        let snapshot = headless_host
            .snapshot("session", "pane")
            .await
            .expect("headless pane");
        assert_eq!(snapshot.rows, 22);
        assert_eq!(snapshot.cols, 90);

        let result = daemon
            .route_command(
                test_runtime_command(
                    "close",
                    Some(1),
                    brehon_types::RuntimeCommandKind::ClosePane {
                        reason: "done".to_string(),
                    },
                ),
                brehon_types::RuntimePolicyContext::default(),
            )
            .await
            .expect("route close");
        assert_eq!(result.status, brehon_types::RuntimeCommandStatus::Applied);
        assert_eq!(
            headless_host
                .snapshot("session", "pane")
                .await
                .expect("headless pane")
                .state,
            brehon_types::RuntimePaneState::Dead
        );
        assert_eq!(
            daemon.pane_registry_snapshot().await.panes[0].state,
            brehon_types::RuntimePaneState::Dead
        );

        runtime_terminal_host
            .as_ref()
            .expect("configured host")
            .shutdown()
            .await
            .expect("host cleanup");
    }

    #[tokio::test]
    async fn headless_run_wiring_smoke_launches_agent_factory_plan() {
        let config = brehon_types::RuntimeTerminalHostConfig {
            kind: Some(brehon_types::RuntimeTerminalHostKind::Headless),
            pane_ownership: Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host),
            ..Default::default()
        };

        let report = run_runtime_terminal_host_wiring_smoke(
            &config,
            "agent-factory-session",
            Path::new("/tmp"),
        )
        .await
        .expect("run wiring smoke");

        assert_eq!(
            report.terminal_host_status.agent_factory,
            brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::TerminalHost
        );
        assert!(report.terminal_host_status.promotion_readiness.ready);
        assert!(report
            .terminal_host_status
            .promotion_readiness
            .blockers
            .is_empty());
        assert_eq!(
            report.spawn_status,
            brehon_types::RuntimeCommandStatus::Applied
        );
        assert_eq!(
            report.resize_status,
            brehon_types::RuntimeCommandStatus::Applied
        );
        assert_eq!(
            report.input_status,
            brehon_types::RuntimeCommandStatus::Applied
        );
        assert_eq!(
            report.reset_status,
            brehon_types::RuntimeCommandStatus::Applied
        );
        assert_eq!(
            report.stale_input_status,
            brehon_types::RuntimeCommandStatus::Rejected
        );
        assert_eq!(
            report.post_reset_input_status,
            brehon_types::RuntimeCommandStatus::Applied
        );
        assert_eq!(
            report.prompt_status,
            brehon_types::RuntimeCommandStatus::Applied
        );
        assert!(report.observed_output);
        assert_eq!(
            report.close_status,
            brehon_types::RuntimeCommandStatus::Applied
        );
        assert_eq!(
            report.post_close_status,
            brehon_types::RuntimeCommandStatus::Rejected
        );
        assert_eq!(report.registry_count, 3);
    }

    #[tokio::test]
    async fn terminal_host_agent_factory_launches_multipane_plan() {
        let config = brehon_types::RuntimeTerminalHostConfig {
            kind: Some(brehon_types::RuntimeTerminalHostKind::Headless),
            pane_ownership: Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host),
            ..Default::default()
        };
        let RuntimeTerminalHostWiring {
            runtime_terminal_host,
            terminal_host_event_forwarder,
            runtime_command_port,
            runtime_command_rx,
            terminal_host_command_port,
            terminal_host_status,
        } = build_runtime_terminal_host_wiring(&config, "factory-session")
            .expect("headless host-owned wiring");
        assert!(runtime_command_rx.is_none());
        assert!(terminal_host_command_port.is_some());

        let mut launch_specs = Vec::new();
        for (pane_id, kind, title) in [
            (
                "supervisor",
                brehon_types::RuntimePaneKind::Supervisor,
                "supervisor",
            ),
            (
                "worker-1",
                brehon_types::RuntimePaneKind::Worker,
                "worker-1",
            ),
            (
                "reviewer-1",
                brehon_types::RuntimePaneKind::Reviewer,
                "reviewer-1",
            ),
        ] {
            let launch = match brehon_mux::AgentTerminalLaunchPlan::from_pty_config(
                "factory-session",
                pane_id,
                Some(title.to_string()),
                kind,
                &brehon_mux::PtyConfig {
                    command: "sh".to_string(),
                    args: vec!["-lc".to_string(), "cat".to_string()],
                    cwd: Some(Path::new("/tmp").to_path_buf()),
                    env: vec![("BREHON_TEST_PANE".to_string(), pane_id.to_string())],
                    rows: 24,
                    cols: 80,
                },
            ) {
                brehon_mux::AgentTerminalLaunchPlan::TerminalHost(launch) => launch,
                _ => unreachable!("pty config launch plans are terminal-host eligible"),
            };
            launch_specs.push(launch);
        }

        let plan = brehon_mux::TerminalHostAgentFactoryPlan {
            total_panes: 3,
            launch_specs,
            blocked_panes: Vec::new(),
        };
        assert!(plan.ready());
        assert_eq!(plan.total_panes, 3);

        let terminal_host_status = terminal_host_status_with_agent_factory_plan_and_owner(
            terminal_host_status,
            &plan,
            brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::TerminalHost,
        );
        assert!(terminal_host_status.promotion_readiness.ready);

        let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
            policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
            command_port: Some(runtime_command_port),
            terminal_host: Some(terminal_host_status),
            ..brehon_daemon::RuntimeDaemonConfig::default()
        });
        let runtime_event_sink: Arc<dyn RuntimeEventSink> = Arc::new(daemon.clone());
        terminal_host_event_forwarder.bind(runtime_event_sink);

        let report = launch_terminal_host_agent_factory_plan(&daemon, "factory-session", &plan)
            .await
            .expect("launch agent factory plan");
        assert_eq!(report.launched, 3);
        assert!(report
            .results
            .iter()
            .all(|result| result.status == brehon_types::RuntimeCommandStatus::Applied));

        let registry = daemon.pane_registry_snapshot().await;
        assert_eq!(registry.panes.len(), 3);
        let mut kinds = registry
            .panes
            .iter()
            .map(|pane| pane.kind.clone())
            .collect::<Vec<_>>();
        kinds.sort_by_key(|kind| format!("{kind:?}"));
        assert_eq!(
            kinds,
            vec![
                brehon_types::RuntimePaneKind::Reviewer,
                brehon_types::RuntimePaneKind::Supervisor,
                brehon_types::RuntimePaneKind::Worker,
            ]
        );

        let brehon_host::ConfiguredTerminalHost::Headless(headless_host) =
            runtime_terminal_host.as_ref().expect("headless host");
        let headless_host = headless_host.clone();
        for pane_id in ["supervisor", "worker-1", "reviewer-1"] {
            let snapshot = headless_host
                .snapshot("factory-session", pane_id)
                .await
                .expect("headless pane snapshot");
            assert_eq!(
                snapshot.command,
                vec!["sh".to_string(), "-lc".to_string(), "cat".to_string()]
            );
            assert_eq!(
                snapshot.env.get("BREHON_TEST_PANE").map(String::as_str),
                Some(pane_id)
            );
        }

        runtime_terminal_host
            .as_ref()
            .expect("configured host")
            .shutdown()
            .await
            .expect("host cleanup");
    }

    #[tokio::test]
    async fn terminal_host_agent_factory_rejects_blocked_plan() {
        let daemon =
            brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig::default());
        let plan = brehon_mux::TerminalHostAgentFactoryPlan {
            total_panes: 1,
            launch_specs: Vec::new(),
            blocked_panes: vec![brehon_mux::TerminalHostAgentFactoryBlockedPane {
                pane_id: "worker-1".to_string(),
                kind: "worker".to_string(),
                reason: "gateway-backed".to_string(),
            }],
        };

        let err = launch_terminal_host_agent_factory_plan(&daemon, "session", &plan)
            .await
            .expect_err("blocked plan should fail before routing commands");

        assert!(err.to_string().contains("plan is not ready"));
        assert!(daemon.pane_registry_snapshot().await.panes.is_empty());
    }

    #[tokio::test]
    async fn terminal_host_agent_factory_rejects_session_mismatch() {
        let daemon =
            brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig::default());
        let launch = match brehon_mux::AgentTerminalLaunchPlan::from_pty_config(
            "other-session",
            "worker-1",
            Some("worker-1".to_string()),
            brehon_types::RuntimePaneKind::Worker,
            &brehon_mux::PtyConfig {
                command: "sh".to_string(),
                args: vec!["-lc".to_string(), "cat".to_string()],
                cwd: Some(Path::new("/tmp").to_path_buf()),
                env: Vec::new(),
                rows: 24,
                cols: 80,
            },
        ) {
            brehon_mux::AgentTerminalLaunchPlan::TerminalHost(launch) => launch,
            _ => unreachable!("pty config launch plans are terminal-host eligible"),
        };
        let plan = brehon_mux::TerminalHostAgentFactoryPlan {
            total_panes: 1,
            launch_specs: vec![launch],
            blocked_panes: Vec::new(),
        };

        let err = launch_terminal_host_agent_factory_plan(&daemon, "session", &plan)
            .await
            .expect_err("cross-session plan should fail before routing commands");

        assert!(err.to_string().contains("targets session 'other-session'"));
        assert!(daemon.pane_registry_snapshot().await.panes.is_empty());
    }

    #[tokio::test]
    async fn terminal_host_preview_pane_uses_host_command_port() {
        let config = brehon_types::RuntimeTerminalHostConfig {
            kind: Some(brehon_types::RuntimeTerminalHostKind::Headless),
            ..Default::default()
        };
        let RuntimeTerminalHostWiring {
            runtime_terminal_host,
            terminal_host_event_forwarder,
            runtime_command_port: _,
            runtime_command_rx: _,
            terminal_host_command_port,
            terminal_host_status,
        } = build_runtime_terminal_host_wiring(&config, "session").expect("headless wiring");
        let brehon_host::ConfiguredTerminalHost::Headless(headless_host) =
            runtime_terminal_host.as_ref().expect("headless host");
        let headless_host = headless_host.clone();
        let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
            terminal_host: Some(terminal_host_status),
            ..brehon_daemon::RuntimeDaemonConfig::default()
        });
        let runtime_event_sink: Arc<dyn RuntimeEventSink> = Arc::new(daemon.clone());
        terminal_host_event_forwarder.bind(runtime_event_sink);

        let result = spawn_terminal_host_preview_pane(
            terminal_host_command_port
                .as_ref()
                .expect("terminal host command port"),
            "session",
            std::path::Path::new("/tmp"),
        )
        .await
        .expect("spawn preview");

        assert_eq!(result.status, brehon_types::RuntimeCommandStatus::Applied);
        let snapshot = headless_host
            .snapshot("session", TERMINAL_HOST_PREVIEW_PANE_ID)
            .await
            .expect("preview pane");
        assert_eq!(snapshot.state, brehon_types::RuntimePaneState::Ready);
        assert_eq!(snapshot.kind, brehon_types::RuntimePaneKind::Shell);
        assert_eq!(snapshot.title.as_deref(), Some("terminal host preview"));

        let registry = daemon.pane_registry_snapshot().await;
        assert_eq!(registry.panes.len(), 1);
        assert_eq!(registry.panes[0].pane_id, TERMINAL_HOST_PREVIEW_PANE_ID);
        assert_eq!(
            registry.panes[0].source,
            Some(brehon_types::RuntimeSource::Headless)
        );
        assert_eq!(registry.panes[0].kind, brehon_types::RuntimePaneKind::Shell);
        let preview_pane =
            terminal_host_preview_pane_from_registry(&registry, "session").expect("preview pane");
        assert_eq!(preview_pane.pane_id, TERMINAL_HOST_PREVIEW_PANE_ID);
        assert_eq!(preview_pane.generation, registry.panes[0].generation);

        let result = close_terminal_host_preview_pane(
            terminal_host_command_port
                .as_ref()
                .expect("terminal host command port"),
            "session",
            &preview_pane,
        )
        .await
        .expect("close preview");
        assert_eq!(result.status, brehon_types::RuntimeCommandStatus::Applied);
        assert_eq!(
            headless_host
                .snapshot("session", TERMINAL_HOST_PREVIEW_PANE_ID)
                .await
                .expect("preview pane")
                .state,
            brehon_types::RuntimePaneState::Dead
        );
        let registry = daemon.pane_registry_snapshot().await;
        assert_eq!(registry.panes.len(), 1);
        assert_eq!(
            registry.panes[0].state,
            brehon_types::RuntimePaneState::Dead
        );

        runtime_terminal_host
            .as_ref()
            .expect("configured host")
            .shutdown()
            .await
            .expect("host cleanup");
    }

    #[test]
    fn supervisor_launch_prefers_lane_model_and_reasoning_effort() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.lanes.insert(
            "codex-supervisor".to_string(),
            brehon_types::LaneConfig {
                launcher: "codex".to_string(),
                model: Some(brehon_types::ModelConfig {
                    provider: "openai".to_string(),
                    name: "gpt-5.4".to_string(),
                }),
                reasoning_effort: Some("high".to_string()),
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "codex-supervisor".to_string();
        config.supervisor.model = Some(brehon_types::ModelConfig {
            provider: "anthropic".to_string(),
            name: "claude-opus-4-6".to_string(),
        });
        config.supervisor.reasoning_effort = Some("low".to_string());

        let format_model = |launcher: &str, provider: &str, model_name: &str| -> String {
            if launcher == "opencode" {
                format!("{provider}/{model_name}")
            } else {
                model_name.to_string()
            }
        };

        assert_eq!(
            resolved_supervisor_model(&config, format_model).as_deref(),
            Some("gpt-5.4")
        );
        assert_eq!(
            resolved_supervisor_reasoning_effort(&config).as_deref(),
            Some("high")
        );
    }

    #[test]
    fn startup_reconciles_dead_assignee_after_review_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("reviews")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            tasks_dir.join("T-orphan-review.json"),
            serde_json::json!({
                "task_id": "T-orphan-review",
                "title": "Recovered review task",
                "task_type": "task",
                "status": "in_review",
                "assignee": serde_json::Value::Null,
                "review_owner": "dead-worker",
                "blockers": "Reviewed commit still does not integrate cleanly. Checkpoint again and re-request review."
            })
            .to_string(),
        )
        .unwrap();

        let first_pass = reconcile_orphaned_worker_assignments_for_run(
            &brehon_root,
            &["live-worker".to_string()],
        )
        .unwrap();
        assert!(first_pass.is_empty(), "{first_pass:?}");

        let config = brehon_config::parse_defaults().unwrap();
        reconcile_review_runtime_for_run(&brehon_root, &[], "supervisor", &config).unwrap();

        let second_pass = reconcile_orphaned_worker_assignments_for_run(
            &brehon_root,
            &["live-worker".to_string()],
        )
        .unwrap();
        assert_eq!(second_pass.len(), 1);

        let repaired_task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tasks_dir.join("T-orphan-review.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(repaired_task["status"], "changes_requested");
        assert!(repaired_task["assignee"].is_null());
        assert_eq!(repaired_task["orphaned_assignee"], "dead-worker");
        assert_eq!(repaired_task["orphaned_status"], "changes_requested");
    }

    #[test]
    fn build_team_member_cwds_includes_supervisor_reviewers_advisors_and_research() {
        let mut worker_cwds = HashMap::new();
        worker_cwds.insert(
            "worker-1".to_string(),
            PathBuf::from("/tmp/workers/worker-1"),
        );
        let mut reviewer_cwds = HashMap::new();
        reviewer_cwds.insert(
            "reviewer-1".to_string(),
            PathBuf::from("/tmp/reviewers/reviewer-1"),
        );
        let mut advisor_cwds = HashMap::new();
        advisor_cwds.insert(
            "advisor-1".to_string(),
            PathBuf::from("/tmp/advisors/advisor-1"),
        );
        let mut research_cwds = HashMap::new();
        research_cwds.insert(
            "research-1".to_string(),
            PathBuf::from("/tmp/research/research-1"),
        );
        let mut supervisor_cwds = HashMap::new();
        supervisor_cwds.insert(
            "claude-code".to_string(),
            PathBuf::from("/tmp/supervisor/claude-code"),
        );

        let member_cwds = build_team_member_cwds(
            &worker_cwds,
            &reviewer_cwds,
            &advisor_cwds,
            &research_cwds,
            "claude-code",
            &supervisor_cwds,
        );

        assert_eq!(
            member_cwds.get("worker-1"),
            Some(&PathBuf::from("/tmp/workers/worker-1"))
        );
        assert_eq!(
            member_cwds.get("reviewer-1"),
            Some(&PathBuf::from("/tmp/reviewers/reviewer-1"))
        );
        assert_eq!(
            member_cwds.get("advisor-1"),
            Some(&PathBuf::from("/tmp/advisors/advisor-1"))
        );
        assert_eq!(
            member_cwds.get("research-1"),
            Some(&PathBuf::from("/tmp/research/research-1"))
        );
        assert_eq!(
            member_cwds.get("claude-code"),
            Some(&PathBuf::from("/tmp/supervisor/claude-code"))
        );
    }

    #[test]
    fn seed_configured_advisor_rooms_writes_participant_names() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = brehon_config::load_config(Some(temp.path())).expect("default config");
        config.advisors = brehon_types::AdvisorConfig {
            enabled: true,
            response_timeout_secs: Some(45),
            default_turn_mode: brehon_types::AdvisorTurnMode::OpenChat,
            pools: vec![brehon_types::AdvisorPoolConfig {
                lane: "codex-worker".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                min: 1,
                max: 1,
                rooms: vec!["release-war-room".to_string()],
                permissions: brehon_types::AdvisorPermissions::ReadOnly,
            }],
            rooms: vec![brehon_types::AdvisorRoomConfig {
                id: "release-war-room".to_string(),
                title: Some("Release War Room".to_string()),
                turn_mode: Some(brehon_types::AdvisorTurnMode::Debate),
                participants: vec!["codex-worker".to_string()],
                context: brehon_types::AdvisorRoomContextConfig {
                    tasks: vec![serde_json::json!({"status": "ready"})],
                    docs: vec!["docs/PHASE5_COMPLETION_HANDOFF.md".to_string()],
                },
            }],
        };
        let advisor_names = vec!["advisor-1".to_string()];
        let mut advisor_agent_type_map = HashMap::new();
        advisor_agent_type_map.insert("advisor-1".to_string(), "codex-worker".to_string());

        seed_configured_advisor_rooms(
            &temp.path().join(".brehon"),
            &config,
            &advisor_names,
            &advisor_agent_type_map,
        )
        .expect("seed advisor rooms");

        let room_path = temp
            .path()
            .join(".brehon/runtime/advisors/rooms/release-war-room.json");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(room_path).unwrap()).unwrap();
        assert_eq!(value["room_id"], "release-war-room");
        assert_eq!(value["turn_mode"], "debate");
        assert_eq!(value["participants"], serde_json::json!(["advisor-1"]));
        assert_eq!(
            value["context"]["docs"],
            serde_json::json!(["docs/PHASE5_COMPLETION_HANDOFF.md"])
        );
        assert!(value["messages"].as_array().is_some());
    }

    #[test]
    fn prepare_runtime_session_state_clears_stale_state_before_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join(".brehon").join("runtime");
        let sessions_dir = runtime_dir.join("sessions");
        let prompt_queue_root = runtime_dir.join("prompt-queue");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&prompt_queue_root).unwrap();
        std::fs::write(sessions_dir.join("old-worker.json"), "{}").unwrap();
        std::fs::write(prompt_queue_root.join("old.entry"), "{}").unwrap();
        std::fs::write(prompt_queue_root.join("old.prompt"), "stale").unwrap();
        std::fs::write(prompt_queue_root.join("old.prompt.retry.json"), "{}").unwrap();

        let stale_count = prepare_runtime_session_state(temp.path(), "brehon-current").unwrap();

        assert_eq!(stale_count, 3);
        assert_eq!(std::fs::read_dir(&sessions_dir).unwrap().count(), 0);
        assert!(prompt_queue_root.join("brehon-current").is_dir());
        assert!(!prompt_queue_root.join("old.entry").exists());
        assert!(!prompt_queue_root.join("old.prompt").exists());
        assert!(!prompt_queue_root.join("old.prompt.retry.json").exists());
        assert_eq!(
            std::fs::read_dir(prompt_queue_root.join("dead-letter"))
                .unwrap()
                .count(),
            3
        );
        let current_session: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(runtime_dir.join("current-session.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(current_session["session_name"], "brehon-current");
        assert!(runtime_dir.join("reviewer-reset-queue").is_dir());
        assert!(runtime_dir.join("reviewer-reset-acks").is_dir());
        assert!(runtime_dir.join("agent-health").is_dir());
    }

    #[tokio::test]
    async fn runtime_daemon_summary_writes_metrics_and_registry() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
            terminal_host: Some(brehon_daemon::RuntimeTerminalHostStatus {
                kind: brehon_types::RuntimeTerminalHostKind::Headless,
                experimental: true,
                observation_running: false,
                command_routing: brehon_daemon::RuntimeTerminalHostCommandRouting::Mux,
                pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
                agent_factory: brehon_daemon::RuntimeTerminalHostAgentFactoryRouting::Mux,
                capabilities: None,
                promotion_readiness: brehon_daemon::RuntimeTerminalHostPromotionReadiness::default(
                ),
                session_name: Some("session-1".to_string()),
                socket_name: None,
                socket_dir: None,
                binary_path: None,
                diagnostics: Vec::new(),
            }),
            ..brehon_daemon::RuntimeDaemonConfig::default()
        });

        brehon_ports::RuntimeEventSink::publish(
            &daemon,
            brehon_types::RuntimeEvent::new(
                brehon_types::RuntimeEventMeta::new(
                    "session-1",
                    "worker-1",
                    3,
                    brehon_types::RuntimeSource::Mux,
                    123,
                ),
                brehon_types::RuntimeEventKind::PaneSpawned(brehon_types::PaneSpawnedEvent {
                    kind: brehon_types::RuntimePaneKind::Worker,
                    title: Some("worker-1".to_string()),
                }),
            ),
        )
        .await
        .unwrap();

        let audit_log_path = brehon_root
            .join("runtime")
            .join("audit")
            .join("session-1.jsonl");
        let path =
            write_runtime_daemon_summary(&brehon_root, "session-1", &audit_log_path, &daemon)
                .await
                .unwrap();
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

        assert_eq!(summary["session_name"], "session-1");
        assert_eq!(
            summary["audit_log_path"],
            audit_log_path.display().to_string()
        );
        assert_eq!(summary["metrics"]["published_events"], 1);
        assert_eq!(summary["registry"]["panes"][0]["pane_id"], "worker-1");
        assert_eq!(summary["terminal_host"]["kind"], "headless");
        assert_eq!(summary["status"]["terminal_host"]["kind"], "headless");
        assert_eq!(
            summary["status"]["registry"]["panes"][0]["pane_id"],
            "worker-1"
        );
    }

    #[tokio::test]
    async fn runtime_daemon_summary_preserves_stopped_state() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let daemon = brehon_daemon::RuntimeDaemon::default();
        daemon.shutdown().await;

        let audit_log_path = brehon_root
            .join("runtime")
            .join("audit")
            .join("session-1.jsonl");
        let path =
            write_runtime_daemon_summary(&brehon_root, "session-1", &audit_log_path, &daemon)
                .await
                .unwrap();
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

        assert_eq!(summary["status"]["running"], false);
    }

    static EXECUTE_DOTBREHON_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn execute_normalizes_dotbrehon_cwd_for_all_project_root_helpers() {
        let _guard = EXECUTE_DOTBREHON_TEST_LOCK.lock().unwrap();

        // Save original env vars so we can restore them after the test.
        let original_root = std::env::var("BREHON_ROOT").ok();
        let original_project = std::env::var("BREHON_PROJECT_ROOT").ok();
        let original_workspace = std::env::var("BREHON_WORKSPACE_ROOT").ok();
        let original_worktree = std::env::var("BREHON_WORKTREE_ROOT").ok();
        let original_session = std::env::var("BREHON_SESSION_NAME").ok();

        let temp = tempfile::tempdir().unwrap();
        let repo_root = temp.path().to_path_buf();
        let brehon_dir = repo_root.join(".brehon");

        std::fs::create_dir_all(&brehon_dir).unwrap();

        // Write a minimal project config that disables worktree isolation so
        // execute gets as far as Mux::factory (which fails without tmux).
        std::fs::write(
            brehon_dir.join("config.yaml"),
            "orchestration:\n  worktree_isolation: false\n",
        )
        .unwrap();

        // Pass an invalid workers override so execute fails deterministically at
        // resolve_worker_pool_counts — after all project-root helpers have run
        // but before Mux::factory (which may succeed in environments with tmux).
        let result = execute(Some(&brehon_dir), None, Some("invalid")).await;

        assert!(
            result.is_err(),
            "execute should fail on invalid workers override, got: {:?}",
            result
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid --workers value"),
            "expected worker-count validation failure, got: {}",
            err_msg
        );

        // BREHON_WORKSPACE_ROOT must point to the repo root, not .brehon/.
        let workspace_root =
            std::env::var("BREHON_WORKSPACE_ROOT").expect("BREHON_WORKSPACE_ROOT should be set");
        assert_eq!(
            workspace_root,
            repo_root.to_string_lossy().to_string(),
            "BREHON_WORKSPACE_ROOT should be the normalized repo root"
        );

        // BREHON_ROOT must point to repo_root/.brehon, not .brehon/.brehon.
        let brehon_root = std::env::var("BREHON_ROOT").expect("BREHON_ROOT should be set");
        assert_eq!(
            brehon_root,
            brehon_dir.to_string_lossy().to_string(),
            "BREHON_ROOT should be repo_root/.brehon"
        );
        let project_root =
            std::env::var("BREHON_PROJECT_ROOT").expect("BREHON_PROJECT_ROOT should be set");
        assert_eq!(
            project_root,
            repo_root.to_string_lossy().to_string(),
            "BREHON_PROJECT_ROOT should be the normalized repo root"
        );
        let worktree_root =
            std::env::var("BREHON_WORKTREE_ROOT").expect("BREHON_WORKTREE_ROOT should be set");
        let resolved_config = brehon_config::load_config(Some(&repo_root)).unwrap();
        assert_eq!(
            worktree_root,
            effective_worktree_root(&repo_root, &resolved_config)
                .to_string_lossy()
                .to_string(),
            "BREHON_WORKTREE_ROOT should match the resolved orchestration worktree root"
        );

        // ensure_mcp_config writes .mcp.json at the project root.
        assert!(
            repo_root.join(".mcp.json").exists(),
            ".mcp.json should be created at repo root"
        );
        assert!(
            !brehon_dir.join(".mcp.json").exists(),
            ".mcp.json should NOT be created inside .brehon"
        );

        // ensure_codex_instruction_files writes under .brehon/instructions/.
        assert!(
            brehon_dir.join("instructions").is_dir(),
            ".brehon/instructions should exist at repo root level"
        );
        assert!(
            !brehon_dir.join(".brehon").exists(),
            ".brehon/.brehon should not be created"
        );

        // Restore original env vars.
        match original_root {
            Some(v) => std::env::set_var("BREHON_ROOT", v),
            None => std::env::remove_var("BREHON_ROOT"),
        }
        match original_project {
            Some(v) => std::env::set_var("BREHON_PROJECT_ROOT", v),
            None => std::env::remove_var("BREHON_PROJECT_ROOT"),
        }
        match original_workspace {
            Some(v) => std::env::set_var("BREHON_WORKSPACE_ROOT", v),
            None => std::env::remove_var("BREHON_WORKSPACE_ROOT"),
        }
        match original_worktree {
            Some(v) => std::env::set_var("BREHON_WORKTREE_ROOT", v),
            None => std::env::remove_var("BREHON_WORKTREE_ROOT"),
        }
        match original_session {
            Some(v) => std::env::set_var("BREHON_SESSION_NAME", v),
            None => std::env::remove_var("BREHON_SESSION_NAME"),
        }
    }

    fn test_runtime_command(
        command_id: &str,
        generation: Option<u64>,
        kind: brehon_types::RuntimeCommandKind,
    ) -> brehon_types::RuntimeCommand {
        brehon_types::RuntimeCommand {
            command_id: command_id.to_string(),
            target: brehon_types::RuntimeCommandTarget {
                session_id: "session".to_string(),
                pane_id: Some("pane".to_string()),
                generation,
            },
            issued_at_ms: 1,
            kind,
        }
    }
}
