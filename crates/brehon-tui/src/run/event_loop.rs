//! Event loop extracted from `run_tui_with_panels`.
//!
//! The `EventLoopCtx` struct bundles all mutable state that lives across loop
//! iterations.  `run()` contains the `while !shutdown` loop body; setup and
//! teardown stay in `mod.rs`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::Terminal;

use brehon_mux::{
    Mux, MuxEvent, MuxRuntimeCommandReceiver, PaneKind, PromptDeliveryAttempt, SessionScopedQueue,
};
use brehon_ports::{PortError, RuntimeCommandRouter};
use brehon_types::config::OrchestrationConfig;
use brehon_types::{
    RuntimeCommand, RuntimeCommandKind, RuntimeCommandStatus, RuntimeCommandTarget, RuntimeEvent,
    RuntimePaneKind, RuntimePaneState, RuntimePolicyContext,
};

use super::advisors::{
    active_advisor_room_id, advisor_room_count, post_operator_advisor_message, render_advisors_view,
};
use super::composer::{
    enqueue_composer_message, handle_composer_key_event, handle_composer_mouse_event,
    handle_composer_paste, render_composer, should_open_composer, worker_mentions_in_message,
    ComposerKeyAction, ComposerSubmission,
};
use super::crash_detection::{
    is_worker_context_reset_candidate, perform_manual_pane_reset, supervisor_reset_reason,
};
use super::dashboard::{
    read_runtime_daemon_dashboard_status, read_task_files, render_dashboard, render_runtime_view,
    RuntimeDaemonDashboardStatus,
};
use super::gateway_prompts::{
    AsyncQueuedGatewayPromptDeliveryTask, QUEUED_GATEWAY_PROMPT_WATCHDOG,
};
use super::helpers::{
    pane_needs_post_spawn_prompt, write_prompt_delivery_ack, write_reviewer_reset_ack,
    write_worker_recycle_ack, ReviewerResetEntry, WorkerRecycleEntry,
};
use super::input::{
    copy_to_clipboard_osc52, extract_selection_text, handle_mouse_input, key_to_plain_char,
    parse_raw_sgr_mouse_sequence, scroll_focused_to_bottom,
};
use super::key_handling::{
    cycle_sub_tab, focused_supervisor_captures_keyboard, is_ctrl_char_key, is_quit_key,
    key_to_bytes, resize_panes as resize_mux_panes, should_handle_key_event,
};
use super::keybind_overlay::{
    handle_keybind_overlay_key_event, handle_keybind_overlay_mouse_event, render_keybind_overlay,
};
use super::layout::{
    calculate_host_owned_layout, calculate_layout, render_3row_tabs, render_group_tabs,
    SUB_TAB_HEIGHT,
};
use super::recovery::{
    clear_agent_health_marker, clear_prompt_retry_meta, dead_letter_prompt_for_session,
    promote_active_assigned_task, push_dashboard_event, queued_prompt_retry_delay,
    record_prompt_retry_deferral, record_prompt_retry_failure,
    should_dead_letter_prompt_after_failure, sync_worker_task_contexts,
};
use super::refresh::{
    apply_dashboard_refresh_snapshot, collect_dashboard_refresh, collect_session_refresh_entries,
    DashboardRefreshSnapshot,
};
use super::rendering::{
    apply_entry_chrome_fade, render_host_owned_pane_in_area,
    render_pane_in_area_with_activity_regions, render_status_bar,
};
use super::research::{
    active_research_room_task_id, post_operator_research_request, render_research_view,
    research_room_count,
};
use super::reviewer_selection::{
    apply_reviewer_selection_state, capture_reviewer_selection_state, focus_current_reviewer,
    ReviewerSelectionState,
};
use super::self_improvement::{
    build_advisor_reset_startup_prompt, build_research_reset_startup_prompt,
    build_reviewer_reset_startup_prompt, build_supervisor_reset_startup_prompt,
    build_worker_context_reset_startup_prompt,
};
use super::session::read_session_files;
use super::task_detail::{handle_task_detail_mouse_event, render_task_detail_dialog};
use super::types::{
    AdvisorRoomViewState, ClickRegion, ClickTarget, ComposerState, DashboardAgentListState,
    DashboardData, DashboardTaskListState, GroupTab, InputMode, PanePos, PendingEscapeSequence,
    RawSgrMouseParse, ResearchRoomViewState, ReviewerPanel, RuntimeCommandActivity, SelectionPane,
    SelectionState, TabEntry, TaskDetailState, RAW_ESCAPE_SEQUENCE_TIMEOUT,
    STALE_ACTIVE_TOOL_THRESHOLD,
};

pub(crate) type PendingRuntimeApprovalResolution = tokio::task::JoinHandle<(
    String,
    bool,
    Result<brehon_types::RuntimeCommandResult, brehon_ports::PortError>,
)>;

pub(crate) struct EventLoopCtx {
    pub shutdown: Arc<AtomicBool>,
    pub mux: Mux,
    pub runtime_command_rx: Option<MuxRuntimeCommandReceiver>,
    pub runtime_event_rx: Option<std::sync::mpsc::Receiver<RuntimeEvent>>,
    pub runtime_command_router: Option<Arc<dyn RuntimeCommandRouter>>,
    pub runtime_agent_factory_host_owned: bool,
    pub runtime_terminal_host_absolute_resize: bool,
    pub rt: tokio::runtime::Handle,
    pub terminal: Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    pub dashboard_data: Arc<std::sync::Mutex<DashboardData>>,
    pub orchestration: OrchestrationConfig,

    pub tick_active: Duration,
    pub tick_idle: Duration,
    pub idle_threshold: Duration,
    pub last_output_at: Instant,
    pub started_at: Instant,

    pub group_tab: GroupTab,
    pub prev_group_tab: GroupTab,
    pub click_regions: Vec<ClickRegion>,
    pub selection: Option<SelectionState>,
    pub pending_down: Option<(SelectionPane, String, PanePos)>,
    pub pending_escape_sequence: Option<PendingEscapeSequence>,
    pub left_pane_area: Rect,
    pub supervisor_pane_area: Rect,

    pub expanded_epics: std::collections::HashSet<String>,
    pub expanded_activity_rows: std::collections::HashSet<(String, String)>,
    pub structured_scroll_offsets: std::collections::HashMap<String, usize>,
    pub input_mode: InputMode,
    pub task_detail: Option<TaskDetailState>,
    pub advisor_room_view: AdvisorRoomViewState,
    pub research_room_view: ResearchRoomViewState,
    pub dashboard_agent_list: DashboardAgentListState,
    pub dashboard_task_list: DashboardTaskListState,
    pub structured_mode: std::collections::HashSet<String>,

    // Stall / recovery tracking
    pub last_activity: std::collections::HashMap<String, Instant>,
    pub auto_recover_threshold: Duration,
    pub worker_context_reset_cooldown: Duration,
    pub self_improve_idle_threshold: Duration,
    pub self_improve_retry_cooldown: Duration,
    pub last_stall_check: Instant,
    pub stall_check_interval: Duration,
    pub supervisor_dispatch_nudge_quiet_threshold: Duration,
    pub supervisor_dispatch_nudge_cooldown: Duration,
    pub last_supervisor_dispatch_nudge: Option<(String, Instant)>,
    pub last_supervisor_reset: std::collections::HashMap<String, Instant>,
    pub last_worker_context_reset: std::collections::HashMap<String, Instant>,
    pub pending_self_improve_prompt: std::collections::HashMap<String, Instant>,
    pub next_self_improve_index: std::collections::HashMap<String, usize>,

    // Post-checkpoint handoff nudge state.
    //
    // Detects the "worker checkpointed with passing tests then went idle
    // waiting for a handoff that never comes" pattern. When the worker's
    // pane has been idle longer than `post_checkpoint_nudge_threshold` AND
    // their assigned task is still `in_progress` AND the task has a
    // `latest_commit` recorded by the last checkpoint, send the worker an
    // explicit prompt reminding them that checkpoint doesn't transition
    // the task — they need `action=complete` or `action=progress`.
    //
    // Nudges are deduplicated by (worker_id, task_id, latest_commit) so
    // the same commit never generates more than one nudge, and a genuine
    // second checkpoint (new commit) can trigger a fresh nudge cycle.
    pub post_checkpoint_nudge_threshold: Duration,
    pub post_checkpoint_nudge_cooldown: Duration,
    pub post_checkpoint_nudges_sent: std::collections::HashMap<(String, String, String), Instant>,
    pub review_obligation_nudges_sent: std::collections::HashMap<(String, String, String), Instant>,
    pub active_worker_recovery_nudges_sent: std::collections::HashMap<(String, String), Instant>,
    pub active_worker_recovery_resets_sent: std::collections::HashMap<(String, String), Instant>,

    // Pane collections
    pub worker_ids: Vec<String>,
    pub all_reviewer_ids: Vec<String>,
    pub advisor_ids: Vec<String>,
    pub research_ids: Vec<String>,
    pub supervisor_id: Option<String>,

    // Reviewer panels
    pub fallback_panels: Vec<ReviewerPanel>,
    pub has_panels: bool,
    pub panels: Vec<ReviewerPanel>,
    pub selected_worker: usize,
    pub selected_panel: usize,
    pub selected_member: Vec<usize>,
    pub reviewer_selection: ReviewerSelectionState,

    // Session / dashboard refresh
    pub pending_initial_resize: bool,
    pub last_session_poll: Instant,
    pub session_poll_interval: Duration,
    pub runtime_session_name: Option<String>,
    pub last_shared_root_issue: Option<String>,
    pub pending_dashboard_refresh: Option<tokio::task::JoinHandle<DashboardRefreshSnapshot>>,
    pub pending_queued_gateway_prompt_deliveries: Vec<AsyncQueuedGatewayPromptDeliveryTask>,
    pub pending_runtime_commands: Vec<PendingRuntimeCommandTask>,
    pub recent_runtime_commands: Vec<RuntimeCommandActivity>,
    pub pending_runtime_approval_resolutions: Vec<PendingRuntimeApprovalResolution>,
    pub entry_chrome_fade_complete: bool,

    /// Loads the merged project `BrehonConfig` on demand. Injected from
    /// `brehon-cli` so this crate can stay free of a `brehon-config` dep.
    pub project_config_loader: super::research::ProjectConfigLoader,

    pub needs_redraw: bool,
}

pub(crate) struct PendingRuntimeCommandTask {
    command_id: String,
    effect: PendingRuntimeCommandEffect,
    handle: tokio::task::JoinHandle<Result<brehon_types::RuntimeCommandResult, PortError>>,
}

pub(crate) enum PendingRuntimeCommandEffect {
    TerminalInput {
        pane_id: String,
    },
    ManualReset {
        pane_id: String,
        startup_prompt: Option<String>,
        success_message: String,
    },
    RecoveryReset {
        pane_id: String,
        startup_prompt: Option<String>,
        success_message: String,
        failure_prefix: String,
        marker: RecoveryResetMarker,
    },
    DashboardAction {
        pane_id: Option<String>,
        success_message: Option<String>,
        failure_prefix: String,
        update_activity: bool,
        clear_pending_self_improve: bool,
    },
    QueuedPromptDelivery {
        path: PathBuf,
        target: String,
        from: Option<String>,
        prompt_id: Option<String>,
        prompt_text: String,
        brehon_root: PathBuf,
        runtime_session_name: Option<String>,
        method: String,
    },
    QueuedReviewerReset {
        request: ReviewerResetEntry,
        startup_prompt: Option<String>,
        brehon_root: PathBuf,
        session_name: String,
    },
    QueuedWorkerRecycle {
        request: WorkerRecycleEntry,
        startup_prompt: Option<String>,
        brehon_root: PathBuf,
        session_name: String,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum RecoveryResetMarker {
    WorkerContext,
    Supervisor,
}

const ENTRY_CHROME_FADE_DURATION: Duration = Duration::from_millis(200);

fn perform_manual_reset_request(ctx: &mut EventLoopCtx, pane_id: &str) -> bool {
    let Some((startup_prompt, success_message)) = manual_reset_plan(ctx, pane_id) else {
        return false;
    };

    let command = RuntimeCommand {
        command_id: format!("manual-reset-{}", uuid::Uuid::new_v4()),
        target: runtime_command_target_for_pane(ctx, pane_id),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::ResetPane {
            reason: "manual reset".to_string(),
        },
    };
    let context = runtime_policy_context_for_pane(ctx, pane_id);

    if queue_runtime_command(
        ctx,
        command,
        context,
        PendingRuntimeCommandEffect::ManualReset {
            pane_id: pane_id.to_string(),
            startup_prompt,
            success_message,
        },
    )
    .is_ok()
    {
        return true;
    }

    if !ctx.runtime_agent_factory_host_owned {
        perform_manual_pane_reset(
            &mut ctx.mux,
            pane_id,
            &ctx.rt,
            &ctx.dashboard_data,
            &mut ctx.last_activity,
            &mut ctx.pending_self_improve_prompt,
        )
    } else {
        push_dashboard_event(
            &ctx.dashboard_data,
            format!("manual reset for {pane_id} failed: runtime command router unavailable"),
        );
        false
    }
}

fn manual_reset_plan(ctx: &EventLoopCtx, pane_id: &str) -> Option<(Option<String>, String)> {
    let pane = ctx.mux.get(pane_id)?;

    match pane.kind() {
        PaneKind::Worker => {
            if !pane.is_gateway_backed() && !ctx.runtime_agent_factory_host_owned {
                let err = format!("manual reset is not supported for non-gateway worker {pane_id}");
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("manual reset for {pane_id} failed: {err}"),
                );
                tracing::warn!(pane = %pane_id, error = %err, "manual reset failed");
                return None;
            }
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
                build_worker_context_reset_startup_prompt(&ctx.mux, pane_id)
            } else {
                None
            };
            Some((startup_prompt, format!("manually reset worker {pane_id}")))
        }
        PaneKind::Reviewer => {
            if pane.review_context().is_some() {
                let err = format!(
                    "cannot manually reset reviewer {pane_id} while an active review is in progress"
                );
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("manual reset for {pane_id} failed: {err}"),
                );
                tracing::warn!(pane = %pane_id, error = %err, "manual reset failed");
                return None;
            }
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
                build_reviewer_reset_startup_prompt(&ctx.mux, pane_id)
            } else {
                None
            };
            Some((startup_prompt, format!("manually reset reviewer {pane_id}")))
        }
        PaneKind::Advisor => {
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
                build_advisor_reset_startup_prompt(&ctx.mux, pane_id)
            } else {
                None
            };
            Some((startup_prompt, format!("manually reset advisor {pane_id}")))
        }
        PaneKind::Research => {
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
                build_research_reset_startup_prompt(&ctx.mux, pane_id)
            } else {
                None
            };
            Some((
                startup_prompt,
                format!("manually reset research agent {pane_id}"),
            ))
        }
        PaneKind::Supervisor => {
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, pane_id) {
                build_supervisor_reset_startup_prompt(&ctx.mux, pane_id)
            } else {
                None
            };
            Some((
                startup_prompt,
                format!("manually reset supervisor {pane_id}"),
            ))
        }
        PaneKind::Director | PaneKind::Shell => {
            let kind = match pane.kind() {
                PaneKind::Director => "director",
                PaneKind::Shell => "shell",
                _ => unreachable!(),
            };
            let err = format!("manual reset is not supported for {kind} pane {pane_id}");
            push_dashboard_event(
                &ctx.dashboard_data,
                format!("manual reset for {pane_id} failed: {err}"),
            );
            tracing::warn!(pane = %pane_id, error = %err, "manual reset failed");
            None
        }
    }
}

pub(super) fn queue_runtime_command(
    ctx: &mut EventLoopCtx,
    command: RuntimeCommand,
    context: RuntimePolicyContext,
    effect: PendingRuntimeCommandEffect,
) -> Result<(), String> {
    let Some(router) = ctx.runtime_command_router.clone() else {
        return Err("runtime command router unavailable".to_string());
    };
    record_runtime_command_pending(ctx, &command);
    let command_id = command.command_id.clone();
    let handle = ctx
        .rt
        .spawn(async move { router.route_command(command, context).await });
    ctx.pending_runtime_commands
        .push(PendingRuntimeCommandTask {
            command_id,
            effect,
            handle,
        });
    Ok(())
}

fn record_runtime_command_pending(ctx: &mut EventLoopCtx, command: &RuntimeCommand) {
    let now_ms = runtime_command_timestamp_ms();
    let activity = RuntimeCommandActivity {
        command_id: command.command_id.clone(),
        label: runtime_command_activity_label(&command.kind).to_string(),
        target: command.target.pane_id.clone(),
        status: "pending".to_string(),
        message: None,
        issued_at_ms: command.issued_at_ms,
        updated_at_ms: now_ms,
    };
    ctx.recent_runtime_commands
        .retain(|existing| existing.command_id != activity.command_id);
    ctx.recent_runtime_commands.insert(0, activity);
    const MAX_RECENT_RUNTIME_COMMANDS: usize = 16;
    if ctx.recent_runtime_commands.len() > MAX_RECENT_RUNTIME_COMMANDS {
        ctx.recent_runtime_commands
            .truncate(MAX_RECENT_RUNTIME_COMMANDS);
    }
}

fn update_runtime_command_activity(
    ctx: &mut EventLoopCtx,
    command_id: &str,
    status: &str,
    message: Option<String>,
) {
    let now_ms = runtime_command_timestamp_ms();
    if let Some(activity) = ctx
        .recent_runtime_commands
        .iter_mut()
        .find(|activity| activity.command_id == command_id)
    {
        activity.status = status.to_string();
        activity.message = message;
        activity.updated_at_ms = now_ms;
    }
}

fn runtime_command_activity_label(kind: &RuntimeCommandKind) -> &'static str {
    match kind {
        RuntimeCommandKind::SendPrompt { .. } => "send-prompt",
        RuntimeCommandKind::BroadcastPrompt { .. } => "broadcast",
        RuntimeCommandKind::SendTerminalInput { .. } => "terminal-input",
        RuntimeCommandKind::Interrupt { .. } => "interrupt",
        RuntimeCommandKind::ResetPane { .. } => "reset",
        RuntimeCommandKind::RecyclePane { .. } => "recycle",
        RuntimeCommandKind::QuarantinePane { .. } => "quarantine",
        RuntimeCommandKind::SpawnPane { .. } => "spawn",
        RuntimeCommandKind::ResizePane { .. } => "resize",
        RuntimeCommandKind::ClosePane { .. } => "close",
        RuntimeCommandKind::ResolveApproval { .. } => "approval",
    }
}

pub(super) fn runtime_command_target_for_pane(
    ctx: &EventLoopCtx,
    pane_id: &str,
) -> RuntimeCommandTarget {
    RuntimeCommandTarget {
        session_id: ctx
            .runtime_session_name
            .as_deref()
            .unwrap_or("default")
            .to_string(),
        pane_id: Some(pane_id.to_string()),
        generation: ctx.mux.get(pane_id).map(|pane| pane.current_generation().0),
    }
}

fn live_worker_ids(ctx: &EventLoopCtx) -> Vec<String> {
    ctx.worker_ids
        .iter()
        .filter(|id| ctx.mux.get(id).is_some())
        .cloned()
        .collect()
}

fn forward_input_bytes(ctx: &mut EventLoopCtx, bytes: &[u8]) {
    ctx.selection = None;
    if bytes.is_empty() {
        return;
    }

    let Some(pane_id) = ctx.mux.focused_id().map(str::to_string) else {
        return;
    };
    ctx.structured_scroll_offsets.remove(&pane_id);
    if let Some(focused) = ctx.mux.focused_mut() {
        if focused.display_scroll_offset() > 0 {
            let _ = focused.scroll_to_bottom();
        }
    }

    // Treat a keystroke as activity for tick-rate purposes. Without this,
    // a quiet pane (>= idle_threshold since the last PTY output) keeps
    // polling on the slow `tick_idle` cadence; the first keypress then
    // waits up to that interval before reaching the child. Bumping the
    // timestamp here forces the active tick on the next iteration so
    // subsequent keystrokes (and the child's echo) are serviced promptly.
    ctx.last_output_at = Instant::now();

    if !ctx.runtime_agent_factory_host_owned {
        super::input::forward_input_bytes(&mut ctx.mux, &ctx.rt, &mut ctx.selection, bytes);
        ctx.last_activity.insert(pane_id, Instant::now());
        return;
    }

    let command = RuntimeCommand {
        command_id: format!("terminal-input-{}", uuid::Uuid::new_v4()),
        target: runtime_command_target_for_pane(ctx, &pane_id),
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::SendTerminalInput {
            bytes: bytes.to_vec(),
        },
    };
    let context = runtime_policy_context_for_pane(ctx, &pane_id);
    if queue_runtime_command(
        ctx,
        command,
        context,
        PendingRuntimeCommandEffect::TerminalInput {
            pane_id: pane_id.clone(),
        },
    )
    .is_ok()
    {
        return;
    }

    tracing::debug!(
        bytes = bytes.len(),
        "ignored direct keyboard input for host-owned external pane"
    );
}

fn should_attach_focused_panesmith_supervisor(
    ctx: &EventLoopCtx,
    key: &crossterm::event::KeyEvent,
) -> bool {
    if ctx.task_detail.is_some() || ctx.runtime_agent_factory_host_owned {
        return false;
    }
    if !is_ctrl_char_key(key, 'f') {
        return false;
    }

    let Some(pane_id) = ctx.mux.focused_id() else {
        return false;
    };
    ctx.mux.is_panesmith_managed(pane_id)
        && ctx
            .mux
            .get(pane_id)
            .is_some_and(|pane| *pane.kind() == PaneKind::Supervisor)
}

fn panesmith_attach_options_for_dashboard() -> panesmith::AttachOptions {
    let mut options = panesmith::AttachOptions::default();
    options.detach.chord = vec![0x06]; // Ctrl-f toggles fullscreen attach off.
    options.screen = panesmith::AttachScreenPolicy::ReuseHostAlternateScreen;
    options
}

#[cfg(unix)]
fn attach_focused_panesmith_supervisor(ctx: &mut EventLoopCtx) -> io::Result<()> {
    let Some(pane_id) = ctx.mux.focused_id().map(str::to_string) else {
        return Ok(());
    };

    ctx.selection = None;
    ctx.pending_down = None;
    ctx.structured_scroll_offsets.remove(&pane_id);
    ctx.click_regions.clear();
    ctx.terminal.backend_mut().flush()?;

    let mut terminal = panesmith::StdioAttachTerminal::new(io::stdout())?;
    let mut control =
        panesmith::CrosstermTerminalControl::new(io::stdout()).with_host_alternate_screen(true);
    match ctx.mux.attach_panesmith_pane_blocking(
        &pane_id,
        panesmith_attach_options_for_dashboard(),
        &mut terminal,
        &mut control,
    ) {
        Ok(outcome) => {
            tracing::info!(
                pane = %pane_id,
                reason = ?outcome.reason,
                child_exit_code = ?outcome.child_exit_code,
                terminal_rows = outcome.terminal_size.rows,
                terminal_cols = outcome.terminal_size.cols,
                restored_rows = outcome.restored_size.rows,
                restored_cols = outcome.restored_size.cols,
                remaining_input = outcome.remaining_input.len(),
                "Panesmith fullscreen attach ended"
            );
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "detached fullscreen supervisor {pane_id}: {:?}",
                    outcome.reason
                ),
            );
            if !outcome.remaining_input.is_empty() {
                forward_input_bytes(ctx, &outcome.remaining_input);
            }
        }
        Err(err) => {
            tracing::warn!(pane = %pane_id, error = %err, "Panesmith fullscreen attach failed");
            push_dashboard_event(
                &ctx.dashboard_data,
                format!("fullscreen attach for {pane_id} failed: {err}"),
            );
        }
    }

    ctx.terminal.clear()?;
    ctx.needs_redraw = true;
    Ok(())
}

#[cfg(not(unix))]
fn attach_focused_panesmith_supervisor(ctx: &mut EventLoopCtx) -> io::Result<()> {
    let pane_id = ctx.mux.focused_id().unwrap_or("focused pane").to_string();
    push_dashboard_event(
        &ctx.dashboard_data,
        format!("fullscreen attach for {pane_id} is only available on Unix terminals"),
    );
    ctx.needs_redraw = true;
    Ok(())
}

fn open_composer(ctx: &mut EventLoopCtx) {
    if ctx.group_tab == GroupTab::Advisors {
        let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
        let room_id = active_advisor_room_id(brehon_root.as_deref());
        let mention_candidates = live_worker_ids(ctx);
        ctx.selection = None;
        ctx.pending_down = None;
        ctx.click_regions.clear();
        ctx.input_mode = InputMode::Composer(
            ComposerState::new_advisor(room_id).with_mention_candidates(mention_candidates),
        );
        return;
    }

    if ctx.group_tab == GroupTab::Research {
        let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
        let task_id = ctx.research_room_view.selected_task_id.clone().or_else(|| {
            active_research_room_task_id(brehon_root.as_deref(), &ctx.project_config_loader)
        });
        ctx.selection = None;
        ctx.pending_down = None;
        ctx.click_regions.clear();
        ctx.input_mode = InputMode::Composer(ComposerState::new_research(task_id));
        return;
    }

    let Some(target) = ctx
        .supervisor_id
        .clone()
        .or_else(|| ctx.mux.supervisor().map(|pane| pane.id().to_string()))
    else {
        push_dashboard_event(
            &ctx.dashboard_data,
            "composer unavailable: no supervisor pane is registered",
        );
        return;
    };
    let task_id = ctx
        .task_detail
        .as_ref()
        .map(|detail| detail.task_id.clone());
    ctx.selection = None;
    ctx.pending_down = None;
    ctx.click_regions.clear();
    ctx.input_mode = InputMode::Composer(ComposerState::new(target, task_id));
}

fn submit_composer_message(ctx: &mut EventLoopCtx, submission: ComposerSubmission) {
    let ComposerSubmission { mut state, message } = submission;
    if let Some(room_id) = state.advisor_room_id().map(str::to_string) {
        let worker_ids = live_worker_ids(ctx);
        let mentioned_workers = match worker_mentions_in_message(&message, &worker_ids) {
            Ok(mentions) => mentions,
            Err(unknown) => {
                state.status = Some(format!(
                    "Unknown live worker mention(s): {}",
                    unknown
                        .iter()
                        .map(|id| format!("@{id}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                state.mention_candidates = worker_ids;
                ctx.input_mode = InputMode::Composer(state);
                return;
            }
        };
        let Some(brehon_root) = ctx.dashboard_data.lock().unwrap().brehon_root.clone() else {
            state.status =
                Some("Cannot post advisor message: .brehon root is unavailable.".to_string());
            ctx.input_mode = InputMode::Composer(state);
            return;
        };
        match post_operator_advisor_message(&brehon_root, &room_id, &message) {
            Ok(seq) => {
                let advisor_ids = ctx.advisor_ids.clone();
                let advisor_count = advisor_ids.len();
                let mut notified = 0usize;
                let mut worker_mentions_notified = 0usize;
                let mut notify_errors = Vec::new();
                if !advisor_ids.is_empty() {
                    for advisor_id in &advisor_ids {
                        let advisor_tool = ctx
                            .mux
                            .get(advisor_id)
                            .map(|pane| {
                                format!("{}advisor", pane.cli_type().capabilities().tool_prefix)
                            })
                            .unwrap_or_else(|| "advisor".to_string());
                        let prompt = format!(
                            "Brehon advisor room update.\n\
Room: {room_id}\n\
New operator message seq: {seq}\n\
\n\
Read the room with `{advisor_tool} action=read room_id={room_id} after_seq={}`. If you have a useful contribution, post it with `{advisor_tool} action=post room_id={room_id} author=<your_name> content=<message>`. Keep the turn short and do not edit files or take task ownership.",
                            seq.saturating_sub(1)
                        );
                        match enqueue_composer_message(
                            &brehon_root,
                            ctx.runtime_session_name.as_deref(),
                            advisor_id,
                            &prompt,
                        ) {
                            Ok(_) => notified += 1,
                            Err(err) => notify_errors.push(format!("{advisor_id}: {err}")),
                        }
                    }
                }
                if !mentioned_workers.is_empty() {
                    for worker_id in &mentioned_workers {
                        let prompt = format!(
                            "Operator mentioned you in advisor room {room_id}, message seq {seq}.\n\
\n\
Message:\n{message}\n\
\n\
This is a notification only: do not change task ownership, do not join the advisor room unless explicitly asked, and respond in your worker pane if a response is needed."
                        );
                        match enqueue_composer_message(
                            &brehon_root,
                            ctx.runtime_session_name.as_deref(),
                            worker_id,
                            &prompt,
                        ) {
                            Ok(_) => worker_mentions_notified += 1,
                            Err(err) => notify_errors.push(format!("{worker_id}: {err}")),
                        }
                    }
                }
                if notified > 0 || worker_mentions_notified > 0 {
                    super::prompt_delivery::deliver_pending_prompts(ctx, &brehon_root);
                }
                let notify_summary = if advisor_count == 0 {
                    "no configured advisor panes".to_string()
                } else if notify_errors.is_empty() {
                    format!("notified {notified} advisor pane(s)")
                } else {
                    format!(
                        "notified {notified} advisor pane(s), {} queue error(s)",
                        notify_errors.len()
                    )
                };
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "posted advisor room message to {room_id} (seq {seq}); {notify_summary}; notified {worker_mentions_notified} mentioned worker(s)"
                    ),
                );
                for error in notify_errors.into_iter().take(3) {
                    push_dashboard_event(
                        &ctx.dashboard_data,
                        format!("advisor notification failed: {error}"),
                    );
                }
                ctx.group_tab = GroupTab::Advisors;
                ctx.needs_redraw = true;
            }
            Err(err) => {
                state.status = Some(format!("Cannot post advisor message: {err}"));
                ctx.input_mode = InputMode::Composer(state);
            }
        }
        return;
    }

    if state.is_research_room() {
        let Some(brehon_root) = ctx.dashboard_data.lock().unwrap().brehon_root.clone() else {
            state.status =
                Some("Cannot queue research request: .brehon root is unavailable.".to_string());
            ctx.input_mode = InputMode::Composer(state);
            return;
        };
        match post_operator_research_request(
            &brehon_root,
            &ctx.project_config_loader,
            state.research_task_id(),
            &message,
            ctx.runtime_session_name.as_deref(),
        ) {
            Ok(result) => {
                if !result.notified_agents.is_empty() {
                    super::prompt_delivery::deliver_pending_prompts(ctx, &brehon_root);
                }
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "queued research request {} for task {}; notified {} research agent(s)",
                        result.job_id,
                        result.task_id,
                        result.notified_agents.len()
                    ),
                );
                ctx.research_room_view.selected_task_id = Some(result.task_id);
                ctx.group_tab = GroupTab::Research;
                ctx.needs_redraw = true;
            }
            Err(err) => {
                state.status = Some(format!("Cannot queue research request: {err}"));
                ctx.input_mode = InputMode::Composer(state);
            }
        }
        return;
    }

    if ctx.mux.get(&state.target).is_none() {
        state.status = Some(format!(
            "Target {} is not available in this run.",
            state.target
        ));
        ctx.input_mode = InputMode::Composer(state);
        return;
    }

    let Some(brehon_root) = ctx.dashboard_data.lock().unwrap().brehon_root.clone() else {
        state.status = Some("Cannot queue directive: .brehon root is unavailable.".to_string());
        ctx.input_mode = InputMode::Composer(state);
        return;
    };

    match enqueue_composer_message(
        &brehon_root,
        ctx.runtime_session_name.as_deref(),
        &state.target,
        &message,
    ) {
        Ok(enqueued) => {
            let id = enqueued.prompt_id.unwrap_or(enqueued.entry_id);
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "queued pending delivery for {} composer directive to {} ({id})",
                    state.workflow.label().to_ascii_lowercase(),
                    state.target
                ),
            );
            super::prompt_delivery::deliver_pending_prompts(ctx, &brehon_root);
            ctx.needs_redraw = true;
        }
        Err(err) => {
            state.status = Some(format!("Cannot queue directive: {err}"));
            ctx.input_mode = InputMode::Composer(state);
        }
    }
}

fn process_pending_runtime_commands(ctx: &mut EventLoopCtx) {
    let mut index = 0usize;
    while index < ctx.pending_runtime_commands.len() {
        if !ctx.pending_runtime_commands[index].handle.is_finished() {
            index += 1;
            continue;
        }

        let task = ctx.pending_runtime_commands.swap_remove(index);
        match ctx.rt.block_on(task.handle) {
            Ok(Ok(result)) => {
                update_runtime_command_activity(
                    ctx,
                    &task.command_id,
                    runtime_command_result_status_label(&result.status),
                    result.message.clone(),
                );
                handle_runtime_command_result(ctx, task.effect, result);
            }
            Ok(Err(err)) => {
                update_runtime_command_activity(
                    ctx,
                    &task.command_id,
                    "failed",
                    Some(err.to_string()),
                );
                handle_runtime_command_error(ctx, task.effect, err.to_string());
            }
            Err(err) => {
                update_runtime_command_activity(
                    ctx,
                    &task.command_id,
                    "failed",
                    Some(err.to_string()),
                );
                handle_runtime_command_error(ctx, task.effect, err.to_string());
            }
        }
        ctx.needs_redraw = true;
    }
}

fn runtime_command_result_status_label(status: &RuntimeCommandStatus) -> &'static str {
    match status {
        RuntimeCommandStatus::Accepted => "accepted",
        RuntimeCommandStatus::Applied => "applied",
        RuntimeCommandStatus::Rejected => "rejected",
        RuntimeCommandStatus::Deferred => "deferred",
    }
}

fn handle_runtime_command_result(
    ctx: &mut EventLoopCtx,
    effect: PendingRuntimeCommandEffect,
    result: brehon_types::RuntimeCommandResult,
) {
    let detail = result
        .message
        .unwrap_or_else(|| format!("{:?}", result.status));
    match result.status {
        RuntimeCommandStatus::Accepted | RuntimeCommandStatus::Applied => {
            handle_runtime_command_success(ctx, effect);
        }
        RuntimeCommandStatus::Deferred => {
            handle_runtime_command_deferred(ctx, effect, detail);
        }
        RuntimeCommandStatus::Rejected => {
            handle_runtime_command_error(ctx, effect, detail);
        }
    }
}

fn handle_runtime_command_deferred(
    ctx: &mut EventLoopCtx,
    effect: PendingRuntimeCommandEffect,
    detail: String,
) {
    match effect {
        PendingRuntimeCommandEffect::QueuedPromptDelivery { path, target, .. } => {
            let retry_after = queued_prompt_retry_delay(1);
            let next_retry_at =
                record_prompt_retry_deferral(&path, retry_after, "daemon prompt delivery deferred");
            super::stall_handling::recover_stale_deferred_prompt_delivery(
                ctx,
                &target,
                &path,
                Instant::now(),
            );
            tracing::info!(
                target = %target,
                detail = %detail,
                next_retry_at = %next_retry_at.to_rfc3339(),
                retry_after_ms = %retry_after.as_millis(),
                "Daemon deferred queued prompt delivery; keeping prompt durable on disk"
            );
        }
        other => handle_runtime_command_error(ctx, other, detail),
    }
}

fn handle_runtime_command_success(ctx: &mut EventLoopCtx, effect: PendingRuntimeCommandEffect) {
    match effect {
        PendingRuntimeCommandEffect::TerminalInput { pane_id } => {
            ctx.last_activity.insert(pane_id, Instant::now());
        }
        PendingRuntimeCommandEffect::ManualReset {
            pane_id,
            startup_prompt,
            success_message,
        } => {
            apply_runtime_reset_success(ctx, &pane_id, startup_prompt, &success_message, None);
        }
        PendingRuntimeCommandEffect::RecoveryReset {
            pane_id,
            startup_prompt,
            success_message,
            marker,
            ..
        } => {
            apply_runtime_reset_success(
                ctx,
                &pane_id,
                startup_prompt,
                &success_message,
                Some(marker),
            );
        }
        PendingRuntimeCommandEffect::DashboardAction {
            pane_id,
            success_message,
            update_activity,
            clear_pending_self_improve,
            ..
        } => {
            if let Some(pane_id) = pane_id {
                if update_activity {
                    ctx.last_activity.insert(pane_id.clone(), Instant::now());
                }
                if clear_pending_self_improve {
                    ctx.pending_self_improve_prompt.remove(&pane_id);
                }
            }
            if let Some(success_message) = success_message {
                push_dashboard_event(&ctx.dashboard_data, success_message);
            }
        }
        PendingRuntimeCommandEffect::QueuedPromptDelivery {
            path,
            target,
            prompt_id,
            brehon_root,
            method,
            ..
        } => {
            ctx.last_activity.insert(target.clone(), Instant::now());
            clear_prompt_retry_meta(&path);
            if let Some(id) = prompt_id.as_deref() {
                if let Err(err) = write_prompt_delivery_ack(&brehon_root, id, &target, &method) {
                    tracing::warn!(
                        target = %target,
                        prompt_id = %id,
                        error = %err,
                        "Failed to persist prompt delivery ack after daemon prompt delivery"
                    );
                }
            }
            tracing::info!(
                target = %target,
                path = %path.display(),
                method = %method,
                "Delivered queued prompt via daemon"
            );
            let _ = std::fs::remove_file(path);
        }
        PendingRuntimeCommandEffect::QueuedReviewerReset {
            request,
            startup_prompt,
            brehon_root,
            ..
        } => {
            if let Some(startup_prompt) = startup_prompt {
                if ctx.runtime_agent_factory_host_owned {
                    if let Err(err) = super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                        ctx,
                        &request.reviewer,
                        startup_prompt,
                        "terminal-host reviewer reset startup prompt",
                    ) {
                        tracing::warn!(
                            reviewer = %request.reviewer,
                            task_id = %request.task_id,
                            error = %err,
                            "Failed to queue terminal-host reviewer reset startup prompt"
                        );
                    }
                } else {
                    ctx.mux
                        .queue_startup_prompt(&request.reviewer, startup_prompt);
                }
            }
            ctx.last_activity
                .insert(request.reviewer.clone(), Instant::now());
            if let Err(err) = write_reviewer_reset_ack(&brehon_root, &request) {
                tracing::warn!(
                    reviewer = %request.reviewer,
                    task_id = %request.task_id,
                    review_id = %request.review_id,
                    error = %err,
                    "Failed to persist reviewer reset acknowledgement"
                );
            } else {
                let reset_reason = request
                    .reason
                    .as_deref()
                    .unwrap_or("reviewer reset after review submission");
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "reset reviewer {} for {} ({})",
                        request.reviewer, request.task_id, reset_reason
                    ),
                );
            }
        }
        PendingRuntimeCommandEffect::QueuedWorkerRecycle {
            request,
            startup_prompt,
            brehon_root,
            ..
        } => {
            ctx.mux.clear_pane_task_context(&request.worker);
            if let Some(startup_prompt) = startup_prompt {
                if ctx.runtime_agent_factory_host_owned {
                    if let Err(err) = super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                        ctx,
                        &request.worker,
                        startup_prompt,
                        "terminal-host worker recycle startup prompt",
                    ) {
                        tracing::warn!(
                            worker = %request.worker,
                            task_id = %request.task_id,
                            error = %err,
                            "Failed to queue terminal-host worker recycle startup prompt"
                        );
                    }
                } else {
                    ctx.mux
                        .queue_startup_prompt(&request.worker, startup_prompt);
                }
            }
            ctx.last_activity
                .insert(request.worker.clone(), Instant::now());
            ctx.pending_self_improve_prompt.remove(&request.worker);
            if let Err(err) = write_worker_recycle_ack(&brehon_root, &request) {
                tracing::warn!(
                    worker = %request.worker,
                    task_id = %request.task_id,
                    error = %err,
                    "Failed to persist worker recycle acknowledgement"
                );
            } else {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "recycled worker {} after terminal handoff for {}",
                        request.worker, request.task_id
                    ),
                );
            }
        }
    }
}

fn handle_runtime_command_error(
    ctx: &mut EventLoopCtx,
    effect: PendingRuntimeCommandEffect,
    detail: String,
) {
    match effect {
        PendingRuntimeCommandEffect::TerminalInput { pane_id } => {
            let message = format!("terminal input for {pane_id} failed: {detail}");
            push_dashboard_event(&ctx.dashboard_data, message.clone());
            tracing::warn!(pane = %pane_id, error = %detail, "{message}");
        }
        PendingRuntimeCommandEffect::ManualReset { pane_id, .. } => {
            let message = format!("manual reset for {pane_id} failed: {detail}");
            push_dashboard_event(&ctx.dashboard_data, message.clone());
            tracing::warn!(pane = %pane_id, error = %detail, "{message}");
        }
        PendingRuntimeCommandEffect::RecoveryReset {
            pane_id,
            failure_prefix,
            ..
        } => {
            let message = format!("{failure_prefix}: {detail}");
            push_dashboard_event(&ctx.dashboard_data, message.clone());
            tracing::warn!(pane = %pane_id, error = %detail, "{message}");
        }
        PendingRuntimeCommandEffect::DashboardAction { failure_prefix, .. } => {
            let message = format!("{failure_prefix}: {detail}");
            push_dashboard_event(&ctx.dashboard_data, message.clone());
            tracing::warn!(error = %detail, "{message}");
        }
        PendingRuntimeCommandEffect::QueuedPromptDelivery {
            path,
            target,
            from,
            prompt_text,
            brehon_root,
            runtime_session_name,
            ..
        } => {
            if should_dead_letter_prompt_after_failure(&prompt_text, &detail) {
                dead_letter_prompt_for_session(
                    &brehon_root,
                    runtime_session_name.as_deref(),
                    &path,
                    &target,
                    from.as_deref(),
                    &prompt_text,
                    &detail,
                    "nonrecoverable daemon prompt delivery failure",
                );
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!(
                        "dead-lettered queued prompt for {target} after daemon delivery failure"
                    ),
                );
                tracing::warn!(
                    target = %target,
                    error = %detail,
                    "Dead-lettered prompt-queue message after daemon delivery failure"
                );
            } else {
                let (attempts, next_retry_at) = record_prompt_retry_failure(&path, &detail);
                tracing::warn!(
                    target = %target,
                    error = %detail,
                    attempts,
                    next_retry_at = %next_retry_at.to_rfc3339(),
                    "Daemon prompt delivery failed; backing off retry"
                );
            }
        }
        PendingRuntimeCommandEffect::QueuedReviewerReset {
            request,
            brehon_root,
            session_name,
            ..
        } => {
            tracing::warn!(
                reviewer = %request.reviewer,
                task_id = %request.task_id,
                review_id = %request.review_id,
                error = %detail,
                "Failed to reset reviewer session from queue"
            );
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "reviewer reset failed for {} on {}; will retry: {}",
                    request.reviewer, request.task_id, detail
                ),
            );
            let reviewer_reset_queue = SessionScopedQueue::<ReviewerResetEntry>::new(
                &session_name,
                brehon_root.join("runtime").join("reviewer-reset-queue"),
            );
            if let Err(requeue_err) = reviewer_reset_queue.enqueue(request.clone()) {
                tracing::warn!(
                    reviewer = %request.reviewer,
                    task_id = %request.task_id,
                    review_id = %request.review_id,
                    error = %requeue_err,
                    "Failed to requeue reviewer reset after reset failure"
                );
            }
        }
        PendingRuntimeCommandEffect::QueuedWorkerRecycle {
            request,
            brehon_root,
            session_name,
            ..
        } => {
            tracing::warn!(
                worker = %request.worker,
                task_id = %request.task_id,
                error = %detail,
                "Failed to reset worker session from recycle queue"
            );
            push_dashboard_event(
                &ctx.dashboard_data,
                format!(
                    "worker recycle failed for {} after {}; will retry: {}",
                    request.worker, request.task_id, detail
                ),
            );
            let worker_recycle_queue = SessionScopedQueue::<WorkerRecycleEntry>::new(
                &session_name,
                brehon_root.join("runtime").join("worker-recycle-queue"),
            );
            if let Err(requeue_err) = worker_recycle_queue.enqueue(request.clone()) {
                tracing::warn!(
                    worker = %request.worker,
                    task_id = %request.task_id,
                    error = %requeue_err,
                    "Failed to requeue worker recycle request after reset failure"
                );
            }
        }
    }
}

fn apply_runtime_reset_success(
    ctx: &mut EventLoopCtx,
    pane_id: &str,
    startup_prompt: Option<String>,
    success_message: &str,
    marker: Option<RecoveryResetMarker>,
) {
    let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
    if let Some(root) = brehon_root.as_ref() {
        clear_agent_health_marker(root, pane_id);
    }
    if let Some(startup_prompt) = startup_prompt {
        if ctx.runtime_agent_factory_host_owned {
            if let Err(err) = super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                ctx,
                pane_id,
                startup_prompt,
                "terminal-host reset startup prompt",
            ) {
                tracing::warn!(
                    pane = %pane_id,
                    error = %err,
                    "Failed to queue terminal-host reset startup prompt"
                );
            }
        } else {
            ctx.mux.queue_startup_prompt(pane_id, startup_prompt);
        }
    }
    let now = Instant::now();
    ctx.last_activity.insert(pane_id.to_string(), now);
    ctx.pending_self_improve_prompt.remove(pane_id);
    if let Some(marker) = marker {
        match marker {
            RecoveryResetMarker::WorkerContext => {
                ctx.last_worker_context_reset
                    .insert(pane_id.to_string(), now);
            }
            RecoveryResetMarker::Supervisor => {
                ctx.last_supervisor_reset.insert(pane_id.to_string(), now);
            }
        }
    }
    push_dashboard_event(&ctx.dashboard_data, success_message.to_string());
    tracing::warn!(pane = %pane_id, "{success_message}");
}

pub(super) fn runtime_policy_context_for_pane(
    ctx: &EventLoopCtx,
    pane_id: &str,
) -> RuntimePolicyContext {
    let pane_state =
        ctx.mux
            .get(pane_id)
            .and_then(|pane| pane.pane_state())
            .map(|state| match state {
                brehon_mux::PaneState::Ready { .. } => RuntimePaneState::Ready,
                brehon_mux::PaneState::Busy { .. } => RuntimePaneState::Busy,
                brehon_mux::PaneState::Dead { .. } => RuntimePaneState::Dead,
            });
    RuntimePolicyContext {
        pane_state,
        ..RuntimePolicyContext::default()
    }
}

fn flush_pending_escape_sequence(ctx: &mut EventLoopCtx) {
    let Some(sequence) = ctx.pending_escape_sequence.take() else {
        return;
    };

    if sequence.bytes == b"\x1b" {
        scroll_focused_to_bottom(&mut ctx.mux, &mut ctx.structured_scroll_offsets);
    } else {
        forward_input_bytes(ctx, &sequence.bytes);
    }
}

fn runtime_dashboard_pane_count(
    runtime_status: Option<&RuntimeDaemonDashboardStatus>,
    kind: RuntimePaneKind,
    fallback: usize,
) -> usize {
    runtime_status
        .map(|status| {
            status
                .registry
                .panes
                .iter()
                .filter(|pane| pane.kind == kind && pane.state != RuntimePaneState::Dead)
                .count()
        })
        .filter(|count| *count > 0)
        .unwrap_or(fallback)
}

fn switch_external_terminal_tab(_ctx: &mut EventLoopCtx, _tab_name: &str) -> bool {
    false
}

fn switch_external_terminal_tab_relative(_ctx: &mut EventLoopCtx, _next: bool) -> bool {
    false
}

fn route_terminal_host_resize(
    ctx: &EventLoopCtx,
    pane_id: &str,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    if rows == 0 || cols == 0 {
        return Ok(());
    }
    let Some(router) = ctx.runtime_command_router.clone() else {
        return Err("runtime command router unavailable".to_string());
    };

    let command = RuntimeCommand {
        command_id: format!("terminal-host-resize-{}", uuid::Uuid::new_v4()),
        target: RuntimeCommandTarget {
            session_id: ctx
                .runtime_session_name
                .as_deref()
                .unwrap_or("default")
                .to_string(),
            pane_id: Some(pane_id.to_string()),
            generation: None,
        },
        issued_at_ms: runtime_command_timestamp_ms(),
        kind: RuntimeCommandKind::ResizePane { rows, cols },
    };

    match ctx
        .rt
        .block_on(router.route_command(command, RuntimePolicyContext::default()))
    {
        Ok(result) if result.status == RuntimeCommandStatus::Applied => Ok(()),
        Ok(result) => Err(result
            .message
            .unwrap_or_else(|| format!("{:?}", result.status))),
        Err(err) => Err(err.to_string()),
    }
}

fn resize_panes(ctx: &mut EventLoopCtx, terminal_size: &Rect) {
    resize_mux_panes(
        &mut ctx.mux,
        terminal_size,
        &ctx.worker_ids,
        &ctx.all_reviewer_ids,
        &ctx.advisor_ids,
        &ctx.research_ids,
        &ctx.supervisor_id,
        ctx.group_tab,
        ctx.has_panels,
    );

    if !ctx.runtime_agent_factory_host_owned || !ctx.runtime_terminal_host_absolute_resize {
        return;
    }

    let areas = calculate_layout(*terminal_size, ctx.group_tab, ctx.has_panels);
    let left_rows = areas.left_content.height.saturating_sub(2);
    let left_cols = areas.left_content.width.saturating_sub(2);
    for pane_id in ctx
        .worker_ids
        .iter()
        .chain(ctx.all_reviewer_ids.iter())
        .chain(ctx.advisor_ids.iter())
        .chain(ctx.research_ids.iter())
    {
        if ctx.mux.get(pane_id).is_none() {
            continue;
        }
        if let Err(err) = route_terminal_host_resize(ctx, pane_id, left_rows, left_cols) {
            push_dashboard_event(
                &ctx.dashboard_data,
                format!("terminal-host resize for {pane_id} failed: {err}"),
            );
            tracing::warn!(
                pane = %pane_id,
                rows = left_rows,
                cols = left_cols,
                error = %err,
                "terminal-host resize command failed"
            );
        }
    }

    if let Some(supervisor_id) = ctx.supervisor_id.as_deref() {
        if ctx.mux.get(supervisor_id).is_some() {
            let supervisor_rows = areas.supervisor_area.height.saturating_sub(2);
            let supervisor_cols = areas.supervisor_area.width.saturating_sub(2);
            if let Err(err) =
                route_terminal_host_resize(ctx, supervisor_id, supervisor_rows, supervisor_cols)
            {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("terminal-host resize for {supervisor_id} failed: {err}"),
                );
                tracing::warn!(
                    pane = %supervisor_id,
                    rows = supervisor_rows,
                    cols = supervisor_cols,
                    error = %err,
                    "terminal-host resize command failed"
                );
            }
        }
    }
}

fn drain_runtime_events_from_daemon(ctx: &mut EventLoopCtx) {
    let Some(runtime_event_rx) = ctx.runtime_event_rx.as_ref() else {
        return;
    };

    let mut events = Vec::new();
    let mut disconnected = false;
    loop {
        match runtime_event_rx.try_recv() {
            Ok(event) => events.push(event),
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                disconnected = true;
                break;
            }
        }
    }
    if disconnected {
        ctx.runtime_event_rx = None;
    }

    if !ctx.runtime_agent_factory_host_owned {
        return;
    }

    for event in events {
        if ctx
            .runtime_session_name
            .as_deref()
            .is_some_and(|session| session != event.meta.session_id)
        {
            continue;
        }
        let pane_id = event.meta.pane_id.clone();
        match ctx.mux.apply_terminal_host_runtime_event(&event) {
            Ok(true) => {
                ctx.last_activity.insert(pane_id, Instant::now());
                ctx.needs_redraw = true;
            }
            Ok(false) => {}
            Err(err) => {
                push_dashboard_event(
                    &ctx.dashboard_data,
                    format!("terminal-host event update for {pane_id} failed: {err}"),
                );
                tracing::warn!(
                    pane = %pane_id,
                    error = %err,
                    "failed to apply terminal-host runtime event"
                );
            }
        }
    }
}

/// Signal returned by `drain_pending_input` describing whether the main
/// event loop should break after this drain pass.
struct InputDrainOutcome {
    should_break: bool,
}

/// Drain any host terminal events (keys, mouse, paste, resize), waiting up
/// to `initial_wait` for the first one. After at least one event arrives,
/// drains everything else that's already queued via repeated
/// `event::poll(Duration::ZERO)` calls.
///
/// Called twice per tick by `run` (see § F8a in
/// tmp/tick-latency/GOAL_PROMPT.md): once at the top of the loop body with
/// `initial_wait = Duration::ZERO` so keystrokes always preempt the
/// output-drain pipeline, and once at the bottom with the active/idle
/// `tick_rate` as the bounded wait. Without the pre-call, every keystroke
/// pays the cost of whatever `mux.poll_batch()` and rendering did this
/// tick — perceived as input lag during heavy streaming.
fn drain_pending_input(
    ctx: &mut EventLoopCtx,
    initial_wait: Duration,
    focused_id: &Option<String>,
    active_left_id: &Option<String>,
) -> io::Result<InputDrainOutcome> {
    let mut outcome = InputDrainOutcome {
        should_break: false,
    };
    if !event::poll(initial_wait)? {
        return Ok(outcome);
    }
    ctx.needs_redraw = true;
    let mut forward_buf: Vec<u8> = Vec::new();
    let mut manual_reset_request: Option<String> = None;
    let mut runtime_approval_request: Option<(String, String, bool)> = None;
    let mut external_terminal_tab_request: Option<String> = None;
    let mut should_break = false;
    loop {
        if !event::poll(Duration::ZERO)? {
            break;
        }
        match event::read()? {
            Event::Key(key) => {
                if !should_handle_key_event(&key) {
                    continue;
                }

                if ctx.pending_escape_sequence.is_some() && !forward_buf.is_empty() {
                    forward_input_bytes(ctx, &forward_buf);
                    forward_buf.clear();
                }
                if let Some(mut pending) = ctx.pending_escape_sequence.take() {
                    match pending.bytes.as_slice() {
                        b"\x1b" => {
                            if key_to_plain_char(&key) == Some('[') {
                                pending.bytes.push(b'[');
                                pending.started_at = Instant::now();
                                ctx.pending_escape_sequence = Some(pending);
                                continue;
                            }

                            scroll_focused_to_bottom(
                                &mut ctx.mux,
                                &mut ctx.structured_scroll_offsets,
                            );
                        }
                        b"\x1b[" => {
                            if key_to_plain_char(&key) == Some('<') {
                                pending.bytes.push(b'<');
                                pending.started_at = Instant::now();
                                ctx.pending_escape_sequence = Some(pending);
                                continue;
                            }

                            forward_input_bytes(ctx, &pending.bytes);
                        }
                        _ => {
                            if let Some(ch) = key_to_plain_char(&key) {
                                pending.bytes.push(ch as u8);
                                match parse_raw_sgr_mouse_sequence(&pending.bytes) {
                                    RawSgrMouseParse::Incomplete => {
                                        pending.started_at = Instant::now();
                                        ctx.pending_escape_sequence = Some(pending);
                                        continue;
                                    }
                                    RawSgrMouseParse::Complete(mouse) => {
                                        if !handle_task_detail_mouse_event(
                                            mouse,
                                            &mut ctx.task_detail,
                                            &mut ctx.selection,
                                            &mut ctx.pending_down,
                                        ) {
                                            let regions_stale = handle_mouse_input(
                                                mouse,
                                                &ctx.click_regions,
                                                &mut ctx.mux,
                                                &mut ctx.group_tab,
                                                &mut ctx.selected_worker,
                                                &mut ctx.selected_panel,
                                                &mut ctx.selected_member,
                                                &ctx.worker_ids,
                                                &ctx.all_reviewer_ids,
                                                &ctx.panels,
                                                &ctx.supervisor_id,
                                                active_left_id,
                                                &mut ctx.expanded_epics,
                                                &mut ctx.expanded_activity_rows,
                                                ctx.left_pane_area,
                                                ctx.supervisor_pane_area,
                                                &mut ctx.selection,
                                                &mut ctx.pending_down,
                                                &mut ctx.task_detail,
                                                &mut ctx.advisor_room_view,
                                                &mut ctx.research_room_view,
                                                &mut ctx.dashboard_agent_list,
                                                &mut ctx.dashboard_task_list,
                                                &ctx.structured_mode,
                                                &mut ctx.structured_scroll_offsets,
                                                ctx.runtime_agent_factory_host_owned,
                                                &mut external_terminal_tab_request,
                                                &mut manual_reset_request,
                                                &mut runtime_approval_request,
                                            );
                                            if regions_stale {
                                                ctx.click_regions.clear();
                                            }
                                            capture_reviewer_selection_state(
                                                &ctx.panels,
                                                ctx.selected_panel,
                                                &ctx.selected_member,
                                                &mut ctx.reviewer_selection,
                                            );
                                        }
                                        continue;
                                    }
                                    RawSgrMouseParse::Invalid => {
                                        forward_input_bytes(ctx, &pending.bytes);
                                        continue;
                                    }
                                }
                            } else {
                                forward_input_bytes(ctx, &pending.bytes);
                            }
                        }
                    }
                }

                if matches!(ctx.input_mode, InputMode::KeybindOverlay(_))
                    && handle_keybind_overlay_key_event(&key, &mut ctx.input_mode)
                {
                    ctx.selection = None;
                    ctx.pending_down = None;
                    continue;
                }

                match handle_composer_key_event(&key, &mut ctx.input_mode) {
                    ComposerKeyAction::Submitted(submission) => {
                        submit_composer_message(ctx, submission);
                        ctx.selection = None;
                        ctx.pending_down = None;
                        continue;
                    }
                    ComposerKeyAction::Handled => {
                        ctx.selection = None;
                        ctx.pending_down = None;
                        continue;
                    }
                    ComposerKeyAction::Ignored => {}
                }

                if should_open_composer(&key) {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    open_composer(ctx);
                    continue;
                }

                if is_quit_key(&key) {
                    should_break = true;
                    break;
                }

                if should_attach_focused_panesmith_supervisor(ctx, &key) {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    attach_focused_panesmith_supervisor(ctx)?;
                    continue;
                }

                if focused_supervisor_captures_keyboard(&ctx.mux, ctx.task_detail.as_ref()) {
                    // Forward Esc immediately when the supervisor PTY
                    // is the focused capture target. The pending-
                    // escape-sequence path existed to disambiguate a
                    // manually-typed SGR mouse sequence (`ESC [ < …`)
                    // from a bare Esc, but `EnableMouseCapture` is
                    // unconditionally on at host startup (see
                    // `run/mod.rs`), so real mouse input arrives as
                    // `Event::Mouse` rather than synthesised keys.
                    // Holding Esc for RAW_ESCAPE_SEQUENCE_TIMEOUT only
                    // added latency to Claude's "cancel current
                    // request" binding, which is exactly the symptom
                    // F5 was filed to fix.
                    if key.code == KeyCode::Esc {
                        forward_buf.push(0x1b);
                    } else if let Some(bytes) = key_to_bytes(&key) {
                        forward_buf.extend_from_slice(&bytes);
                    }
                    continue;
                }

                if let Some(ref mut detail) = ctx.task_detail {
                    match key.code {
                        KeyCode::Esc => {
                            ctx.task_detail = None;
                            ctx.selection = None;
                            ctx.pending_down = None;
                            ctx.click_regions.clear();
                        }
                        KeyCode::Up => {
                            detail.scroll = detail.scroll.saturating_sub(1);
                        }
                        KeyCode::Down => {
                            detail.scroll = (detail.scroll + 1).min(detail.max_scroll);
                        }
                        KeyCode::PageUp => {
                            detail.scroll = detail.scroll.saturating_sub(8);
                        }
                        KeyCode::PageDown => {
                            detail.scroll = (detail.scroll + 8).min(detail.max_scroll);
                        }
                        KeyCode::Home => {
                            detail.scroll = 0;
                        }
                        KeyCode::End => {
                            detail.scroll = detail.max_scroll;
                        }
                        KeyCode::Char('k') if key.modifiers.is_empty() => {
                            detail.scroll = detail.scroll.saturating_sub(1);
                        }
                        KeyCode::Char('j') if key.modifiers.is_empty() => {
                            detail.scroll = (detail.scroll + 1).min(detail.max_scroll);
                        }
                        KeyCode::Char('r') if key.modifiers.is_empty() => {
                            ctx.research_room_view.selected_task_id = Some(detail.task_id.clone());
                            ctx.research_room_view.scroll = 0;
                            ctx.task_detail = None;
                            ctx.group_tab = GroupTab::Research;
                            ctx.click_regions.clear();
                        }
                        _ => {}
                    }
                } else if handle_keybind_overlay_key_event(&key, &mut ctx.input_mode) {
                    ctx.selection = None;
                    ctx.pending_down = None;
                } else if ctx.group_tab == GroupTab::Dashboard
                    && matches!(
                        key.code,
                        KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::PageUp
                            | KeyCode::PageDown
                            | KeyCode::Home
                            | KeyCode::End
                            | KeyCode::Char('j')
                            | KeyCode::Char('k')
                    )
                {
                    match key.code {
                        KeyCode::Up => {
                            ctx.dashboard_task_list.scroll =
                                ctx.dashboard_task_list.scroll.saturating_sub(1);
                        }
                        KeyCode::Down => {
                            ctx.dashboard_task_list.scroll = (ctx.dashboard_task_list.scroll + 1)
                                .min(ctx.dashboard_task_list.max_scroll);
                        }
                        KeyCode::PageUp => {
                            ctx.dashboard_task_list.scroll =
                                ctx.dashboard_task_list.scroll.saturating_sub(8);
                        }
                        KeyCode::PageDown => {
                            ctx.dashboard_task_list.scroll = (ctx.dashboard_task_list.scroll + 8)
                                .min(ctx.dashboard_task_list.max_scroll);
                        }
                        KeyCode::Home => {
                            ctx.dashboard_task_list.scroll = 0;
                        }
                        KeyCode::End => {
                            ctx.dashboard_task_list.scroll = ctx.dashboard_task_list.max_scroll;
                        }
                        KeyCode::Char('k') if key.modifiers.is_empty() => {
                            ctx.dashboard_task_list.scroll =
                                ctx.dashboard_task_list.scroll.saturating_sub(1);
                        }
                        KeyCode::Char('j') if key.modifiers.is_empty() => {
                            ctx.dashboard_task_list.scroll = (ctx.dashboard_task_list.scroll + 1)
                                .min(ctx.dashboard_task_list.max_scroll);
                        }
                        _ => {}
                    }
                } else if (ctx.group_tab == GroupTab::Advisors
                    || ctx.group_tab == GroupTab::Research)
                    && matches!(
                        key.code,
                        KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::PageUp
                            | KeyCode::PageDown
                            | KeyCode::Home
                            | KeyCode::End
                            | KeyCode::Char('j')
                            | KeyCode::Char('k')
                    )
                {
                    match key.code {
                        KeyCode::Up => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll =
                                    ctx.advisor_room_view.scroll.saturating_sub(1);
                            } else {
                                ctx.research_room_view.scroll =
                                    ctx.research_room_view.scroll.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll = (ctx.advisor_room_view.scroll + 1)
                                    .min(ctx.advisor_room_view.max_scroll);
                            } else {
                                ctx.research_room_view.scroll = (ctx.research_room_view.scroll + 1)
                                    .min(ctx.research_room_view.max_scroll);
                            }
                        }
                        KeyCode::PageUp => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll =
                                    ctx.advisor_room_view.scroll.saturating_sub(8);
                            } else {
                                ctx.research_room_view.scroll =
                                    ctx.research_room_view.scroll.saturating_sub(8);
                            }
                        }
                        KeyCode::PageDown => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll = (ctx.advisor_room_view.scroll + 8)
                                    .min(ctx.advisor_room_view.max_scroll);
                            } else {
                                ctx.research_room_view.scroll = (ctx.research_room_view.scroll + 8)
                                    .min(ctx.research_room_view.max_scroll);
                            }
                        }
                        KeyCode::Home => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll = 0;
                            } else {
                                ctx.research_room_view.scroll = 0;
                            }
                        }
                        KeyCode::End => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll = ctx.advisor_room_view.max_scroll;
                            } else {
                                ctx.research_room_view.scroll = ctx.research_room_view.max_scroll;
                            }
                        }
                        KeyCode::Char('k') if key.modifiers.is_empty() => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll =
                                    ctx.advisor_room_view.scroll.saturating_sub(1);
                            } else {
                                ctx.research_room_view.scroll =
                                    ctx.research_room_view.scroll.saturating_sub(1);
                            }
                        }
                        KeyCode::Char('j') if key.modifiers.is_empty() => {
                            if ctx.group_tab == GroupTab::Advisors {
                                ctx.advisor_room_view.scroll = (ctx.advisor_room_view.scroll + 1)
                                    .min(ctx.advisor_room_view.max_scroll);
                            } else {
                                ctx.research_room_view.scroll = (ctx.research_room_view.scroll + 1)
                                    .min(ctx.research_room_view.max_scroll);
                            }
                        }
                        _ => {}
                    }
                } else if key.code == KeyCode::BackTab {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.click_regions.clear();
                    if ctx.runtime_agent_factory_host_owned {
                        switch_external_terminal_tab_relative(ctx, false);
                        continue;
                    }
                    cycle_sub_tab(
                        &mut ctx.mux,
                        ctx.group_tab,
                        &ctx.worker_ids,
                        &ctx.panels,
                        &mut ctx.selected_worker,
                        &mut ctx.selected_panel,
                        &mut ctx.selected_member,
                        -1,
                    );
                    capture_reviewer_selection_state(
                        &ctx.panels,
                        ctx.selected_panel,
                        &ctx.selected_member,
                        &mut ctx.reviewer_selection,
                    );
                } else if is_ctrl_char_key(&key, ']') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.click_regions.clear();
                    if ctx.runtime_agent_factory_host_owned {
                        switch_external_terminal_tab_relative(ctx, true);
                        continue;
                    }
                    cycle_sub_tab(
                        &mut ctx.mux,
                        ctx.group_tab,
                        &ctx.worker_ids,
                        &ctx.panels,
                        &mut ctx.selected_worker,
                        &mut ctx.selected_panel,
                        &mut ctx.selected_member,
                        1,
                    );
                    capture_reviewer_selection_state(
                        &ctx.panels,
                        ctx.selected_panel,
                        &ctx.selected_member,
                        &mut ctx.reviewer_selection,
                    );
                } else if is_ctrl_char_key(&key, 'd') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.group_tab = GroupTab::Dashboard;
                    ctx.click_regions.clear();
                } else if is_ctrl_char_key(&key, 't') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.group_tab = GroupTab::Runtime;
                    ctx.click_regions.clear();
                } else if is_ctrl_char_key(&key, 'a') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.group_tab = GroupTab::Advisors;
                    ctx.click_regions.clear();
                } else if is_ctrl_char_key(&key, 'r') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.group_tab = GroupTab::Research;
                    ctx.click_regions.clear();
                } else if is_ctrl_char_key(&key, 'v') {
                    if let Some(ref focused_id) = focused_id {
                        if let Some(pane) = ctx.mux.get(focused_id) {
                            if pane.is_gateway_backed() {
                                if ctx.structured_mode.contains(focused_id) {
                                    ctx.structured_mode.remove(focused_id);
                                } else {
                                    ctx.structured_mode.insert(focused_id.clone());
                                }
                                ctx.click_regions.clear();
                            }
                        }
                    }
                } else if is_ctrl_char_key(&key, 'w') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.click_regions.clear();
                    if ctx.runtime_agent_factory_host_owned {
                        switch_external_terminal_tab(ctx, "Workers");
                        continue;
                    }
                    ctx.group_tab = GroupTab::Workers;
                    if let Some(id) = ctx.worker_ids.get(ctx.selected_worker) {
                        ctx.mux.focus(id);
                    }
                } else if is_ctrl_char_key(&key, 'e') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.click_regions.clear();
                    if ctx.runtime_agent_factory_host_owned {
                        switch_external_terminal_tab(ctx, "Reviewers");
                        continue;
                    }
                    ctx.group_tab = GroupTab::Reviewers;
                    focus_current_reviewer(
                        &mut ctx.mux,
                        &ctx.panels,
                        ctx.selected_panel,
                        &ctx.selected_member,
                    );
                } else if is_ctrl_char_key(&key, 's') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.selection = None;
                    ctx.task_detail = None;
                    ctx.click_regions.clear();
                    if ctx.runtime_agent_factory_host_owned {
                        switch_external_terminal_tab(ctx, "Supervisor");
                        continue;
                    }
                    if let Some(ref sup_id) = ctx.supervisor_id {
                        ctx.mux.focus(sup_id);
                    }
                } else if is_ctrl_char_key(&key, 'r') {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    if let Some(focused_id) = ctx.mux.focused_id().map(str::to_string) {
                        if perform_manual_reset_request(ctx, &focused_id) {
                            ctx.needs_redraw = true;
                        }
                    }
                } else if key.code == KeyCode::Esc {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    ctx.pending_escape_sequence = Some(PendingEscapeSequence {
                        started_at: Instant::now(),
                        bytes: vec![0x1b],
                    });
                } else if key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                    && key.code == KeyCode::Char('C')
                {
                    if let Some(ref sel) = ctx.selection {
                        let pane_ok = ctx
                            .mux
                            .get(&sel.pane_id)
                            .map(|p| {
                                !ctx.structured_mode.contains(&sel.pane_id)
                                    || !p.is_gateway_backed()
                            })
                            .unwrap_or(true);
                        if pane_ok {
                            let text = extract_selection_text(sel, &ctx.mux);
                            if !text.is_empty() {
                                let mut stdout = io::stdout();
                                let _ = copy_to_clipboard_osc52(&text, &mut stdout);
                                tracing::debug!(
                                    len = text.len(),
                                    "Copied selection to clipboard via Ctrl+Shift+C"
                                );
                            }
                        }
                    }
                } else if let Some(bytes) = key_to_bytes(&key) {
                    forward_buf.extend_from_slice(&bytes);
                }
            }
            Event::Mouse(mouse)
                if !handle_composer_mouse_event(mouse, &mut ctx.input_mode)
                    && !handle_keybind_overlay_mouse_event(mouse, &mut ctx.input_mode)
                    && !handle_task_detail_mouse_event(
                        mouse,
                        &mut ctx.task_detail,
                        &mut ctx.selection,
                        &mut ctx.pending_down,
                    ) =>
            {
                let regions_stale = handle_mouse_input(
                    mouse,
                    &ctx.click_regions,
                    &mut ctx.mux,
                    &mut ctx.group_tab,
                    &mut ctx.selected_worker,
                    &mut ctx.selected_panel,
                    &mut ctx.selected_member,
                    &ctx.worker_ids,
                    &ctx.all_reviewer_ids,
                    &ctx.panels,
                    &ctx.supervisor_id,
                    active_left_id,
                    &mut ctx.expanded_epics,
                    &mut ctx.expanded_activity_rows,
                    ctx.left_pane_area,
                    ctx.supervisor_pane_area,
                    &mut ctx.selection,
                    &mut ctx.pending_down,
                    &mut ctx.task_detail,
                    &mut ctx.advisor_room_view,
                    &mut ctx.research_room_view,
                    &mut ctx.dashboard_agent_list,
                    &mut ctx.dashboard_task_list,
                    &ctx.structured_mode,
                    &mut ctx.structured_scroll_offsets,
                    ctx.runtime_agent_factory_host_owned,
                    &mut external_terminal_tab_request,
                    &mut manual_reset_request,
                    &mut runtime_approval_request,
                );
                if regions_stale {
                    ctx.click_regions.clear();
                }
                capture_reviewer_selection_state(
                    &ctx.panels,
                    ctx.selected_panel,
                    &ctx.selected_member,
                    &mut ctx.reviewer_selection,
                );
            }
            Event::Paste(text) => {
                if handle_composer_paste(&text, &mut ctx.input_mode) {
                    continue;
                }
                if !matches!(ctx.input_mode, InputMode::Normal) {
                    ctx.input_mode = InputMode::Normal;
                    continue;
                }
                if !forward_buf.is_empty() {
                    forward_input_bytes(ctx, &forward_buf);
                    forward_buf.clear();
                }
                // Bracketed paste: forward entire pasted text to the PTY at once,
                // wrapped in bracketed paste sequences so the underlying CLI
                // (OpenCode, Codex, etc.) treats it as a single paste, not
                // individual keystrokes with newlines interpreted as Enter.
                let mut payload = Vec::with_capacity(text.len() + 12);
                payload.extend_from_slice(b"\x1b[200~");
                payload.extend_from_slice(text.as_bytes());
                payload.extend_from_slice(b"\x1b[201~");
                forward_input_bytes(ctx, &payload);
            }
            Event::Resize(cols, rows) => {
                ctx.selection = None;
                let size = Rect::new(0, 0, cols, rows);
                resize_panes(ctx, &size);
            }
            _ => {}
        }
    } // drain loop
    if let Some(pane_id) = manual_reset_request.take() {
        if perform_manual_reset_request(ctx, &pane_id) {
            ctx.needs_redraw = true;
        }
    }
    if let Some(tab_name) = external_terminal_tab_request.take() {
        switch_external_terminal_tab(ctx, &tab_name);
    }
    if let Some((approval_id, session_id, approved)) = runtime_approval_request.take() {
        if let Some(router) = ctx.runtime_command_router.clone() {
            let command = RuntimeCommand {
                command_id: format!("runtime-approval-{}", uuid::Uuid::new_v4()),
                target: RuntimeCommandTarget {
                    session_id,
                    pane_id: None,
                    generation: None,
                },
                issued_at_ms: runtime_command_timestamp_ms(),
                kind: RuntimeCommandKind::ResolveApproval {
                    approval_id: approval_id.clone(),
                    approved,
                },
            };
            ctx.pending_runtime_approval_resolutions
                .push(ctx.rt.spawn(async move {
                    let result = router
                        .route_command(command, RuntimePolicyContext::default())
                        .await;
                    (approval_id, approved, result)
                }));
            ctx.needs_redraw = true;
        } else {
            push_dashboard_event(
                &ctx.dashboard_data,
                format!("cannot resolve runtime approval {approval_id}: router unavailable"),
            );
        }
    }
    // Flush batched input in a single PTY write to reduce per-keystroke overhead.
    if !forward_buf.is_empty() {
        forward_input_bytes(ctx, &forward_buf);
    }
    outcome.should_break = should_break;
    Ok(outcome)
}

pub(super) fn run(ctx: &mut EventLoopCtx) -> io::Result<()> {
    while !ctx.shutdown.load(Ordering::Relaxed) {
        let mut runtime_command_rx_disconnected = false;
        if let Some(runtime_command_rx) = ctx.runtime_command_rx.as_mut() {
            loop {
                match runtime_command_rx.try_recv() {
                    Ok(request) => {
                        let result = ctx
                            .mux
                            .execute_runtime_command(&ctx.rt, request.command().clone());
                        request.complete(result);
                        ctx.needs_redraw = true;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        runtime_command_rx_disconnected = true;
                        break;
                    }
                }
            }
        }
        if runtime_command_rx_disconnected {
            ctx.runtime_command_rx = None;
        }
        drain_runtime_events_from_daemon(ctx);

        // Pre-drain: service any keystrokes that already accumulated so
        // they don't wait behind the (potentially long) PTY output
        // pipeline below. See § F8a in tmp/tick-latency/GOAL_PROMPT.md.
        // `focused_id` / `active_left_id` are recomputed because the
        // canonical versions (used by the render path) aren't in scope
        // yet at this point in the tick — they're built post-render
        // around line 1949 / 1965 below.
        let pre_focused_id = ctx.mux.focused_id().map(str::to_string);
        let pre_active_left_id: Option<String> = match ctx.group_tab {
            GroupTab::Dashboard | GroupTab::Runtime | GroupTab::Advisors | GroupTab::Research => {
                None
            }
            GroupTab::Workers => ctx.worker_ids.get(ctx.selected_worker).cloned(),
            GroupTab::Reviewers => ctx
                .panels
                .get(ctx.selected_panel)
                .and_then(|p| {
                    let mi = ctx
                        .selected_member
                        .get(ctx.selected_panel)
                        .copied()
                        .unwrap_or(0);
                    p.members.get(mi)
                })
                .cloned(),
        };
        let pre_drain =
            drain_pending_input(ctx, Duration::ZERO, &pre_focused_id, &pre_active_left_id)?;
        if pre_drain.should_break {
            break;
        }

        let (_total_bytes, batch_events) = ctx.mux.poll_batch();
        ctx.mux.flush_pending_inbox_nudges(&ctx.rt);
        let loop_now = std::time::Instant::now();

        process_pending_runtime_commands(ctx);

        let mut approval_resolution_index = 0usize;
        while approval_resolution_index < ctx.pending_runtime_approval_resolutions.len() {
            if ctx.pending_runtime_approval_resolutions[approval_resolution_index].is_finished() {
                let task = ctx
                    .pending_runtime_approval_resolutions
                    .swap_remove(approval_resolution_index);
                match ctx.rt.block_on(task) {
                    Ok((approval_id, approved, Ok(result))) => {
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!(
                                "{} runtime approval {}: {:?}",
                                if approved { "approved" } else { "denied" },
                                approval_id,
                                result.status
                            ),
                        );
                        if let Some(message) = result.message {
                            tracing::info!(
                                approval_id = %approval_id,
                                approved,
                                message = %message,
                                "Resolved runtime approval"
                            );
                        }
                    }
                    Ok((approval_id, approved, Err(err))) => {
                        push_dashboard_event(
                            &ctx.dashboard_data,
                            format!(
                                "failed to {} runtime approval {}: {}",
                                if approved { "approve" } else { "deny" },
                                approval_id,
                                err
                            ),
                        );
                        tracing::warn!(
                            approval_id = %approval_id,
                            approved,
                            error = %err,
                            "Failed to resolve runtime approval"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "Runtime approval resolution task failed"
                        );
                    }
                }
                ctx.needs_redraw = true;
            } else {
                approval_resolution_index += 1;
            }
        }

        if ctx
            .pending_dashboard_refresh
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(handle) = ctx.pending_dashboard_refresh.take() {
                match ctx.rt.block_on(handle) {
                    Ok(snapshot) => {
                        apply_dashboard_refresh_snapshot(
                            &mut ctx.mux,
                            &ctx.dashboard_data,
                            &mut ctx.panels,
                            &mut ctx.selected_panel,
                            &mut ctx.selected_member,
                            &mut ctx.reviewer_selection,
                            &mut ctx.last_shared_root_issue,
                            snapshot,
                        );
                        ctx.needs_redraw = true;
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "Dashboard refresh task failed");
                    }
                }
            }
        }

        let mut queued_gateway_completion_index = 0usize;
        while queued_gateway_completion_index < ctx.pending_queued_gateway_prompt_deliveries.len() {
            if ctx.pending_queued_gateway_prompt_deliveries[queued_gateway_completion_index]
                .handle
                .is_finished()
            {
                let task = ctx
                    .pending_queued_gateway_prompt_deliveries
                    .swap_remove(queued_gateway_completion_index);
                let result = match ctx.rt.block_on(task.handle) {
                    Ok(result) => result,
                    Err(err) => Err(brehon_mux::AsyncGatewayPromptDeliveryError {
                        error: err.to_string(),
                    }),
                };
                match result {
                    Ok(PromptDeliveryAttempt::Delivered { .. }) => {
                        ctx.mux.finalize_async_gateway_prompt_delivery(
                            &task.target,
                            &task.prompt_text,
                            task.from.as_deref(),
                            Ok(()),
                        );
                        ctx.last_activity
                            .insert(task.target.clone(), Instant::now());
                        clear_prompt_retry_meta(&task.path);
                        if let (Some(id), Some(root)) = (
                            task.prompt_id.as_deref(),
                            ctx.dashboard_data.lock().unwrap().brehon_root.clone(),
                        ) {
                            if let Err(err) = super::helpers::write_prompt_delivery_ack(
                                &root,
                                id,
                                &task.target,
                                "gateway",
                            ) {
                                tracing::warn!(
                                    target = %task.target,
                                    prompt_id = %id,
                                    error = %err,
                                    "Failed to persist prompt delivery ack after gateway delivery"
                                );
                            }
                        }
                        let _ = std::fs::remove_file(&task.path);
                        tracing::info!(
                            target = %task.target,
                            "Delivered queued gateway prompt via background attempt"
                        );
                    }
                    Ok(PromptDeliveryAttempt::Queued {
                        prompt_id,
                        ahead_of,
                    }) => {
                        if let Some(generation) = ctx
                            .mux
                            .get(&task.target)
                            .map(|pane| pane.current_generation())
                        {
                            ctx.mux.mark_gateway_delivery_busy(
                                &task.target,
                                prompt_id.clone(),
                                generation,
                            );
                        }
                        let retry_after = queued_prompt_retry_delay(ahead_of);
                        let next_retry_at = record_prompt_retry_deferral(
                            &task.path,
                            retry_after,
                            "gateway delivery queued prompt",
                        );
                        super::stall_handling::recover_stale_deferred_prompt_delivery(
                            ctx,
                            &task.target,
                            &task.path,
                            Instant::now(),
                        );
                        tracing::info!(
                            target = %task.target,
                            prompt_id = %prompt_id,
                            ahead_of,
                            next_retry_at = %next_retry_at.to_rfc3339(),
                            retry_after_ms = %retry_after.as_millis(),
                            "Queued gateway prompt after background attempt"
                        );
                    }
                    Ok(PromptDeliveryAttempt::AlreadyPresent {
                        prompt_id,
                        position,
                    }) => {
                        let retry_after = queued_prompt_retry_delay(position.retry_ahead_of());
                        let next_retry_at = record_prompt_retry_deferral(
                            &task.path,
                            retry_after,
                            "gateway delivery prompt already queued",
                        );
                        super::stall_handling::recover_stale_deferred_prompt_delivery(
                            ctx,
                            &task.target,
                            &task.path,
                            Instant::now(),
                        );
                        tracing::info!(
                            target = %task.target,
                            prompt_id = %prompt_id,
                            position = %position,
                            next_retry_at = %next_retry_at.to_rfc3339(),
                            retry_after_ms = %retry_after.as_millis(),
                            "Queued gateway prompt already present after background attempt"
                        );
                    }
                    Ok(PromptDeliveryAttempt::Rejected { reason }) => {
                        let err_text = format!("prompt delivery rejected: {reason:?}");
                        ctx.mux.finalize_async_gateway_prompt_delivery(
                            &task.target,
                            &task.prompt_text,
                            task.from.as_deref(),
                            Err(brehon_mux::AsyncGatewayPromptDeliveryError {
                                error: err_text.clone(),
                            }),
                        );
                        let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
                        let Some(root) = brehon_root.as_ref() else {
                            tracing::warn!(
                                target = %task.target,
                                error = %err_text,
                                "Queued gateway prompt rejected without BREHON_ROOT available for retry handling"
                            );
                            continue;
                        };
                        if should_dead_letter_prompt_after_failure(&task.prompt_text, &err_text) {
                            dead_letter_prompt_for_session(
                                root,
                                ctx.runtime_session_name.as_deref(),
                                &task.path,
                                &task.target,
                                task.from.as_deref(),
                                &task.prompt_text,
                                &err_text,
                                "nonrecoverable prompt delivery rejection",
                            );
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "dead-lettered queued prompt for {} after gateway delivery rejection",
                                    task.target
                                ),
                            );
                            tracing::warn!(
                                target = %task.target,
                                error = %err_text,
                                "Dead-lettered queued gateway prompt after delivery rejection"
                            );
                        } else {
                            let (attempts, next_retry_at) =
                                record_prompt_retry_failure(&task.path, &err_text);
                            tracing::warn!(
                                target = %task.target,
                                error = %err_text,
                                attempts,
                                next_retry_at = %next_retry_at.to_rfc3339(),
                                "Rejected queued gateway prompt delivery; backing off retry"
                            );
                        }
                    }
                    Err(err) => {
                        let err_text = err.error;
                        ctx.mux.finalize_async_gateway_prompt_delivery(
                            &task.target,
                            &task.prompt_text,
                            task.from.as_deref(),
                            Err(brehon_mux::AsyncGatewayPromptDeliveryError {
                                error: err_text.clone(),
                            }),
                        );
                        let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
                        let Some(root) = brehon_root.as_ref() else {
                            tracing::warn!(
                                target = %task.target,
                                error = %err_text,
                                "Queued gateway prompt failed without BREHON_ROOT available for retry handling"
                            );
                            continue;
                        };
                        if should_dead_letter_prompt_after_failure(&task.prompt_text, &err_text) {
                            dead_letter_prompt_for_session(
                                root,
                                ctx.runtime_session_name.as_deref(),
                                &task.path,
                                &task.target,
                                task.from.as_deref(),
                                &task.prompt_text,
                                &err_text,
                                "nonrecoverable prompt delivery failure",
                            );
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "dead-lettered queued prompt for {} after gateway delivery failure",
                                    task.target
                                ),
                            );
                            tracing::warn!(
                                target = %task.target,
                                error = %err_text,
                                "Dead-lettered queued gateway prompt after nonrecoverable failure"
                            );
                        } else {
                            let (attempts, next_retry_at) =
                                record_prompt_retry_failure(&task.path, &err_text);
                            tracing::warn!(
                                target = %task.target,
                                error = %err_text,
                                attempts,
                                next_retry_at = %next_retry_at.to_rfc3339(),
                                "Failed queued gateway prompt delivery; backing off retry"
                            );
                        }
                    }
                }
                ctx.needs_redraw = true;
            } else {
                let elapsed = ctx.pending_queued_gateway_prompt_deliveries
                    [queued_gateway_completion_index]
                    .started_at
                    .elapsed();
                if elapsed >= QUEUED_GATEWAY_PROMPT_WATCHDOG {
                    let task = ctx
                        .pending_queued_gateway_prompt_deliveries
                        .swap_remove(queued_gateway_completion_index);
                    task.handle.abort();
                    ctx.mux.finalize_async_gateway_prompt_delivery(
                        &task.target,
                        &task.prompt_text,
                        task.from.as_deref(),
                        Err(brehon_mux::AsyncGatewayPromptDeliveryError {
                            error: "watchdog aborted stuck queued gateway prompt delivery"
                                .to_string(),
                        }),
                    );
                    let (attempts, next_retry_at) = record_prompt_retry_failure(
                        &task.path,
                        "watchdog aborted stuck queued gateway prompt delivery",
                    );
                    tracing::warn!(
                        target = %task.target,
                        path = %task.path.display(),
                        elapsed_ms = %elapsed.as_millis(),
                        attempts,
                        next_retry_at = %next_retry_at.to_rfc3339(),
                        "Watchdog aborted stuck queued gateway prompt delivery; backing off retry"
                    );
                    ctx.needs_redraw = true;
                } else {
                    queued_gateway_completion_index += 1;
                }
            }
        }

        // Mark dirty when new PTY output arrives.
        if !batch_events.is_empty() {
            ctx.needs_redraw = true;
            ctx.last_output_at = Instant::now();
        }

        apply_reviewer_selection_state(
            &ctx.panels,
            &mut ctx.reviewer_selection,
            &mut ctx.selected_panel,
            &mut ctx.selected_member,
        );

        // Track per-pane activity for stall detection
        for ev in &batch_events {
            match ev {
                MuxEvent::PaneOutput { pane_id, .. }
                | MuxEvent::ActivityEvent { pane_id, .. }
                | MuxEvent::ActivityFlush { pane_id, .. } => {
                    ctx.last_activity
                        .insert(pane_id.clone(), std::time::Instant::now());
                    ctx.pending_self_improve_prompt.remove(pane_id);
                }
                _ => {}
            }
        }

        let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
        if let Some(root) = brehon_root.as_ref() {
            let mut active_worker_panes = std::collections::BTreeSet::new();
            for ev in &batch_events {
                match ev {
                    MuxEvent::PaneOutput { pane_id, .. }
                    | MuxEvent::ActivityEvent { pane_id, .. }
                    | MuxEvent::ActivityFlush { pane_id, .. } => {
                        active_worker_panes.insert(pane_id.clone());
                    }
                    _ => {}
                }
            }

            let mut promoted_tasks = Vec::new();
            for pane_id in active_worker_panes {
                let Some(pane) = ctx.mux.get(&pane_id) else {
                    continue;
                };
                if *pane.kind() != PaneKind::Worker {
                    continue;
                }
                let Some(task_id) = pane.task_context().map(|context| context.task_id.clone())
                else {
                    continue;
                };
                if let Some(new_status) = promote_active_assigned_task(root, &task_id, &pane_id) {
                    promoted_tasks.push((pane_id, task_id, new_status.to_string()));
                }
            }

            if !promoted_tasks.is_empty() {
                let refreshed_tasks = read_task_files(root);
                let refreshed_sessions = read_session_files(root);
                sync_worker_task_contexts(&mut ctx.mux, &refreshed_tasks, &refreshed_sessions);
                ctx.dashboard_data.lock().unwrap().tasks = refreshed_tasks;
                ctx.needs_redraw = true;
                for (worker, task_id, new_status) in promoted_tasks {
                    tracing::info!(
                        worker = %worker,
                        task_id = %task_id,
                        status = %new_status,
                        "Promoted assigned task after worker activity"
                    );
                }
            }
        }

        let mut context_reset_candidates = std::collections::BTreeSet::new();
        for ev in &batch_events {
            if let MuxEvent::ActivityEvent { pane_id, entry, .. } = ev {
                if is_worker_context_reset_candidate(&ctx.mux, pane_id, entry) {
                    context_reset_candidates.insert(pane_id.clone());
                }
            }
        }
        for pane_id in context_reset_candidates {
            if ctx
                .last_worker_context_reset
                .get(&pane_id)
                .is_some_and(|last| {
                    loop_now.duration_since(*last) < ctx.worker_context_reset_cooldown
                })
            {
                continue;
            }
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, &pane_id) {
                let Some(startup_prompt) =
                    build_worker_context_reset_startup_prompt(&ctx.mux, &pane_id)
                else {
                    continue;
                };
                Some(startup_prompt)
            } else {
                None
            };
            let task_id = ctx
                .mux
                .get(&pane_id)
                .and_then(|pane| pane.task_context().map(|task| task.task_id.clone()));
            let summary = match task_id.as_deref() {
                Some(task_id) => format!(
                    "reset worker {pane_id} after provider/runtime failure while continuing {task_id}"
                ),
                None => {
                    format!("reset worker {pane_id} after provider/runtime failure")
                }
            };
            let command = RuntimeCommand {
                command_id: format!("worker-context-reset-{}", uuid::Uuid::new_v4()),
                target: runtime_command_target_for_pane(ctx, &pane_id),
                issued_at_ms: runtime_command_timestamp_ms(),
                kind: RuntimeCommandKind::ResetPane {
                    reason: "provider/runtime failure reset".to_string(),
                },
            };
            let context = runtime_policy_context_for_pane(ctx, &pane_id);
            if queue_runtime_command(
                ctx,
                command,
                context,
                PendingRuntimeCommandEffect::RecoveryReset {
                    pane_id: pane_id.clone(),
                    startup_prompt: startup_prompt.clone(),
                    success_message: summary.clone(),
                    failure_prefix: format!(
                        "failed to reset worker {pane_id} after provider/runtime failure"
                    ),
                    marker: RecoveryResetMarker::WorkerContext,
                },
            )
            .is_ok()
            {
                ctx.needs_redraw = true;
                continue;
            }

            let reset_result = if ctx.runtime_agent_factory_host_owned {
                super::prompt_delivery::reset_terminal_host_pane(
                    ctx,
                    &pane_id,
                    "provider/runtime failure reset",
                )
            } else {
                ctx.rt
                    .block_on(ctx.mux.reset_worker_gateway_session(&pane_id))
                    .map_err(|err| err.to_string())
            };
            match reset_result {
                Ok(()) => {
                    if let Some(startup_prompt) = startup_prompt {
                        if ctx.runtime_agent_factory_host_owned {
                            if let Err(err) =
                                super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                                    ctx,
                                    &pane_id,
                                    startup_prompt,
                                    "terminal-host worker context reset startup prompt",
                                )
                            {
                                tracing::warn!(
                                    pane = %pane_id,
                                    error = %err,
                                    "Failed to queue terminal-host worker reset startup prompt"
                                );
                            }
                        } else {
                            ctx.mux.queue_startup_prompt(&pane_id, startup_prompt);
                        }
                    }
                    ctx.last_worker_context_reset
                        .insert(pane_id.clone(), loop_now);
                    ctx.last_activity.insert(pane_id.clone(), loop_now);
                    ctx.needs_redraw = true;
                    push_dashboard_event(&ctx.dashboard_data, summary.clone());
                    tracing::warn!(pane = %pane_id, task_id = ?task_id, "{summary}");
                }
                Err(err) => {
                    tracing::warn!(
                        pane = %pane_id,
                        error = %err,
                        "Failed to reset worker after provider/runtime failure"
                    );
                }
            }
        }

        let supervisor_reset_cooldown = Duration::from_secs(60);
        let mut supervisor_reset_candidates = std::collections::BTreeMap::new();
        for ev in &batch_events {
            match ev {
                MuxEvent::PaneExited { pane_id, .. } | MuxEvent::PaneOutput { pane_id, .. } => {
                    if let Some(reason) = supervisor_reset_reason(&ctx.mux, pane_id) {
                        supervisor_reset_candidates
                            .entry(pane_id.clone())
                            .or_insert(reason);
                    }
                }
                _ => {}
            }
        }
        for (pane_id, reason) in supervisor_reset_candidates {
            if ctx
                .last_supervisor_reset
                .get(&pane_id)
                .is_some_and(|last| loop_now.duration_since(*last) < supervisor_reset_cooldown)
            {
                continue;
            }
            let startup_prompt = if pane_needs_post_spawn_prompt(&ctx.mux, &pane_id) {
                let Some(startup_prompt) =
                    build_supervisor_reset_startup_prompt(&ctx.mux, &pane_id)
                else {
                    continue;
                };
                Some(startup_prompt)
            } else {
                None
            };
            let summary = format!(
                "reset supervisor {pane_id} after detected {reason}; reloading coordination context"
            );
            let reset_reason = format!("supervisor runtime failure: {reason}");
            let command = RuntimeCommand {
                command_id: format!("supervisor-reset-{}", uuid::Uuid::new_v4()),
                target: runtime_command_target_for_pane(ctx, &pane_id),
                issued_at_ms: runtime_command_timestamp_ms(),
                kind: RuntimeCommandKind::ResetPane {
                    reason: reset_reason.clone(),
                },
            };
            let context = runtime_policy_context_for_pane(ctx, &pane_id);
            if queue_runtime_command(
                ctx,
                command,
                context,
                PendingRuntimeCommandEffect::RecoveryReset {
                    pane_id: pane_id.clone(),
                    startup_prompt: startup_prompt.clone(),
                    success_message: summary.clone(),
                    failure_prefix: format!(
                        "failed to reset supervisor {pane_id} after detected {reason}"
                    ),
                    marker: RecoveryResetMarker::Supervisor,
                },
            )
            .is_ok()
            {
                ctx.needs_redraw = true;
                continue;
            }

            let reset_result = if ctx.runtime_agent_factory_host_owned {
                super::prompt_delivery::reset_terminal_host_pane(ctx, &pane_id, reset_reason)
            } else {
                ctx.rt
                    .block_on(ctx.mux.reset_supervisor_session(&pane_id))
                    .map_err(|err| err.to_string())
            };
            match reset_result {
                Ok(()) => {
                    if let Some(startup_prompt) = startup_prompt {
                        if ctx.runtime_agent_factory_host_owned {
                            if let Err(err) =
                                super::prompt_delivery::enqueue_terminal_host_startup_prompt(
                                    ctx,
                                    &pane_id,
                                    startup_prompt,
                                    "terminal-host supervisor reset startup prompt",
                                )
                            {
                                tracing::warn!(
                                    pane = %pane_id,
                                    error = %err,
                                    "Failed to queue terminal-host supervisor reset startup prompt"
                                );
                            }
                        } else {
                            ctx.mux.queue_startup_prompt(&pane_id, startup_prompt);
                        }
                    }
                    ctx.last_supervisor_reset.insert(pane_id.clone(), loop_now);
                    ctx.last_activity.insert(pane_id.clone(), loop_now);
                    ctx.needs_redraw = true;
                    push_dashboard_event(&ctx.dashboard_data, summary.clone());
                    tracing::warn!(pane = %pane_id, reason, "{summary}");
                }
                Err(err) => {
                    tracing::warn!(
                        pane = %pane_id,
                        reason,
                        error = %err,
                        "Failed to reset supervisor after detected runtime failure"
                    );
                }
            }
        }

        // Determine which left pane is active
        let active_left_id: Option<String> = match ctx.group_tab {
            GroupTab::Dashboard | GroupTab::Runtime | GroupTab::Advisors | GroupTab::Research => {
                None
            } // these views have no pane
            GroupTab::Workers => ctx.worker_ids.get(ctx.selected_worker).cloned(),
            GroupTab::Reviewers => ctx
                .panels
                .get(ctx.selected_panel)
                .and_then(|p| {
                    let mi = ctx
                        .selected_member
                        .get(ctx.selected_panel)
                        .copied()
                        .unwrap_or(0);
                    p.members.get(mi)
                })
                .cloned(),
        };
        let focused_id = ctx.mux.focused_id().map(str::to_string);
        let dashboard_snapshot = ctx.dashboard_data.lock().unwrap().clone();
        let runtime_status = if ctx.runtime_agent_factory_host_owned {
            dashboard_snapshot
                .brehon_root
                .as_deref()
                .and_then(read_runtime_daemon_dashboard_status)
        } else {
            None
        };

        // Resize PTYs when group tab changes (different tabs have different content area heights).
        if ctx.group_tab != ctx.prev_group_tab {
            if let Ok(size) = ctx.terminal.size() {
                let terminal_size = size.into();
                resize_panes(ctx, &terminal_size);
            }
            ctx.prev_group_tab = ctx.group_tab;
        }

        if !ctx.entry_chrome_fade_complete {
            if ctx.started_at.elapsed() < ENTRY_CHROME_FADE_DURATION {
                ctx.needs_redraw = true;
            } else {
                ctx.entry_chrome_fade_complete = true;
                ctx.needs_redraw = true;
            }
        }

        // Keep redrawing while any structured pane shows an active tool timer.
        if !ctx.needs_redraw {
            for id in ctx.structured_mode.iter() {
                if let Some(pane) = ctx.mux.get(id) {
                    if let Some(buf) = pane.activity_buffer() {
                        if buf.active_tools().next().is_some() {
                            ctx.needs_redraw = true;
                            break;
                        }
                    }
                }
            }
        }

        if ctx.needs_redraw {
            if ctx.pending_initial_resize {
                if let Ok(size) = ctx.terminal.size() {
                    let terminal_size = size.into();
                    resize_panes(ctx, &terminal_size);
                }
                ctx.pending_initial_resize = false;
            }
            ctx.terminal.draw(|f| {
                let areas = if ctx.runtime_agent_factory_host_owned {
                    calculate_host_owned_layout(f.area(), ctx.group_tab, ctx.has_panels)
                } else {
                    calculate_layout(f.area(), ctx.group_tab, ctx.has_panels)
                };
                let mut regions = Vec::new();
                let visible_worker_count = if ctx.runtime_agent_factory_host_owned {
                    runtime_dashboard_pane_count(
                        runtime_status.as_ref(),
                        RuntimePaneKind::Worker,
                        ctx.worker_ids.len(),
                    )
                } else {
                    ctx.worker_ids.len()
                };
                let visible_reviewer_count = if ctx.runtime_agent_factory_host_owned {
                    runtime_dashboard_pane_count(
                        runtime_status.as_ref(),
                        RuntimePaneKind::Reviewer,
                        ctx.all_reviewer_ids.len(),
                    )
                } else {
                    ctx.all_reviewer_ids.len()
                };
                let visible_advisor_count = if ctx.runtime_agent_factory_host_owned {
                    runtime_dashboard_pane_count(
                        runtime_status.as_ref(),
                        RuntimePaneKind::Advisor,
                        ctx.advisor_ids.len(),
                    )
                } else {
                    ctx.advisor_ids.len()
                }
                .max(advisor_room_count(
                    dashboard_snapshot.brehon_root.as_deref(),
                ));
                let visible_research_count = research_room_count(
                    dashboard_snapshot.brehon_root.as_deref(),
                    &ctx.project_config_loader,
                );

                // 1. Group tab bar
                let gr = render_group_tabs(
                    f,
                    areas.group_tab_bar,
                    ctx.group_tab,
                    visible_worker_count,
                    visible_reviewer_count,
                    visible_advisor_count,
                    visible_research_count,
                );
                regions.extend(gr);

                // 2. Left tab stack (not used for Dashboard)
                match ctx.group_tab {
                    GroupTab::Dashboard => {
                        // No sub-tabs for dashboard
                    }
                    GroupTab::Runtime => {
                        // No sub-tabs for runtime
                    }
                    GroupTab::Advisors => {
                        // No sub-tabs for advisor rooms
                    }
                    GroupTab::Research => {
                        // No sub-tabs for research rooms
                    }
                    GroupTab::Workers => {
                        // Single 3-row sub-tab bar for workers
                        let tabs: Vec<TabEntry> = ctx
                            .worker_ids
                            .iter()
                            .enumerate()
                            .map(|(i, id)| TabEntry {
                                id: id.clone(),
                                label: format!(" {} ", id),
                                is_selected: i == ctx.selected_worker,
                            })
                            .collect();
                        let sub_area = Rect::new(
                            areas.left_tab_stack.x,
                            areas.left_tab_stack.y,
                            areas.left_tab_stack.width,
                            SUB_TAB_HEIGHT.min(areas.left_tab_stack.height),
                        );
                        let sr = render_3row_tabs(f, sub_area, &tabs, |id| {
                            ClickTarget::SubTab(id.to_string())
                        });
                        regions.extend(sr);
                    }
                    GroupTab::Reviewers => {
                        if ctx.has_panels {
                            // Panel tabs (first 3-row bar)
                            let panel_tabs: Vec<TabEntry> = ctx
                                .panels
                                .iter()
                                .enumerate()
                                .map(|(i, p)| TabEntry {
                                    id: p.name.clone(),
                                    label: format!(" {} ({}) ", p.name, p.members.len()),
                                    is_selected: i == ctx.selected_panel,
                                })
                                .collect();
                            let panel_area = Rect::new(
                                areas.left_tab_stack.x,
                                areas.left_tab_stack.y,
                                areas.left_tab_stack.width,
                                SUB_TAB_HEIGHT.min(areas.left_tab_stack.height),
                            );
                            let pr = render_3row_tabs(f, panel_area, &panel_tabs, |id| {
                                ClickTarget::SubTab(id.to_string())
                            });
                            regions.extend(pr);

                            // Member tabs (second 3-row bar)
                            if let Some(panel) = ctx.panels.get(ctx.selected_panel) {
                                let mi = ctx
                                    .selected_member
                                    .get(ctx.selected_panel)
                                    .copied()
                                    .unwrap_or(0);
                                let member_tabs: Vec<TabEntry> = panel
                                    .members
                                    .iter()
                                    .enumerate()
                                    .map(|(i, id)| TabEntry {
                                        id: id.clone(),
                                        label: format!(" {} ", id),
                                        is_selected: i == mi,
                                    })
                                    .collect();
                                let member_y = areas.left_tab_stack.y + SUB_TAB_HEIGHT;
                                let member_h = areas
                                    .left_tab_stack
                                    .height
                                    .saturating_sub(SUB_TAB_HEIGHT)
                                    .min(SUB_TAB_HEIGHT);
                                if member_h > 0 {
                                    let member_area = Rect::new(
                                        areas.left_tab_stack.x,
                                        member_y,
                                        areas.left_tab_stack.width,
                                        member_h,
                                    );
                                    let mr = render_3row_tabs(f, member_area, &member_tabs, |id| {
                                        ClickTarget::MemberTab(id.to_string())
                                    });
                                    regions.extend(mr);
                                }
                            }
                        } else {
                            // Flat reviewer tabs (single bar, same as workers)
                            let tabs: Vec<TabEntry> = ctx
                                .all_reviewer_ids
                                .iter()
                                .enumerate()
                                .map(|(i, id)| TabEntry {
                                    id: id.clone(),
                                    label: format!(" {} ", id),
                                    is_selected: {
                                        let mi = ctx.selected_member.first().copied().unwrap_or(0);
                                        i == mi
                                    },
                                })
                                .collect();
                            let sub_area = Rect::new(
                                areas.left_tab_stack.x,
                                areas.left_tab_stack.y,
                                areas.left_tab_stack.width,
                                SUB_TAB_HEIGHT.min(areas.left_tab_stack.height),
                            );
                            let sr = render_3row_tabs(f, sub_area, &tabs, |id| {
                                ClickTarget::SubTab(id.to_string())
                            });
                            regions.extend(sr);
                        }
                    }
                }

                // 3. Left content: dashboard or selected pane
                if ctx.group_tab == GroupTab::Dashboard {
                    let epic_regions = render_dashboard(
                        f,
                        areas.left_content,
                        &ctx.mux,
                        &dashboard_snapshot,
                        &mut ctx.expanded_epics,
                        &mut ctx.dashboard_agent_list,
                        &mut ctx.dashboard_task_list,
                        &ctx.recent_runtime_commands,
                        ctx.started_at.elapsed().as_millis() as usize / 90,
                    );
                    regions.extend(epic_regions);
                } else if ctx.group_tab == GroupTab::Runtime {
                    let runtime_regions = render_runtime_view(
                        f,
                        areas.left_content,
                        dashboard_snapshot.brehon_root.as_deref(),
                        &ctx.recent_runtime_commands,
                    );
                    regions.extend(runtime_regions);
                } else if ctx.group_tab == GroupTab::Advisors {
                    render_advisors_view(
                        f,
                        areas.left_content,
                        dashboard_snapshot.brehon_root.as_deref(),
                        &mut ctx.advisor_room_view,
                    );
                } else if ctx.group_tab == GroupTab::Research {
                    render_research_view(
                        f,
                        areas.left_content,
                        dashboard_snapshot.brehon_root.as_deref(),
                        &ctx.project_config_loader,
                        &mut ctx.research_room_view,
                    );
                } else if let Some(ref left_id) = active_left_id {
                    let is_focused = focused_id.as_deref() == Some(left_id.as_str());
                    let is_structured = ctx.structured_mode.contains(left_id);
                    let reset_rect = if ctx.runtime_agent_factory_host_owned {
                        render_host_owned_pane_in_area(
                            f,
                            areas.left_content,
                            &ctx.mux,
                            left_id,
                            is_focused,
                            runtime_status.as_ref(),
                        )
                    } else {
                        render_pane_in_area_with_activity_regions(
                            f,
                            areas.left_content,
                            &ctx.mux,
                            left_id,
                            is_focused,
                            ctx.selection
                                .as_ref()
                                .filter(|s| s.pane == SelectionPane::Left),
                            is_structured,
                            &ctx.expanded_activity_rows,
                            ctx.structured_scroll_offsets.get(left_id).copied(),
                            Some(&mut regions),
                        )
                    };
                    if let Some(rect) = reset_rect {
                        regions.push(ClickRegion {
                            rect,
                            target: ClickTarget::ResetPane(left_id.clone()),
                        });
                    }
                    regions.push(ClickRegion {
                        rect: areas.left_content,
                        target: ClickTarget::LeftPane,
                    });
                }

                // 4. Supervisor (embedded/mux-owned panes only)
                if !ctx.runtime_agent_factory_host_owned {
                    if let Some(ref sup_id) = ctx.supervisor_id {
                        let is_focused = focused_id.as_deref() == Some(sup_id.as_str());
                        let is_structured = ctx.structured_mode.contains(sup_id);
                        let reset_rect = render_pane_in_area_with_activity_regions(
                            f,
                            areas.supervisor_area,
                            &ctx.mux,
                            sup_id,
                            is_focused,
                            ctx.selection
                                .as_ref()
                                .filter(|s| s.pane == SelectionPane::Supervisor),
                            is_structured,
                            &ctx.expanded_activity_rows,
                            ctx.structured_scroll_offsets.get(sup_id).copied(),
                            Some(&mut regions),
                        );
                        if let Some(rect) = reset_rect {
                            regions.push(ClickRegion {
                                rect,
                                target: ClickTarget::ResetPane(sup_id.clone()),
                            });
                        }
                        regions.push(ClickRegion {
                            rect: areas.supervisor_area,
                            target: ClickTarget::SupervisorPane,
                        });
                    }
                }

                // 5. Status bar
                render_status_bar(f, areas.status_bar, &ctx.mux, &ctx.last_activity, loop_now);

                if ctx.group_tab == GroupTab::Dashboard {
                    if let Some(ref mut detail) = ctx.task_detail {
                        render_task_detail_dialog(f, f.area(), &dashboard_snapshot, detail);
                    }
                }
                if let InputMode::KeybindOverlay(ref mut overlay) = ctx.input_mode {
                    render_keybind_overlay(f, f.area(), overlay);
                }
                if let InputMode::Composer(ref mut composer) = ctx.input_mode {
                    let composer_area = if composer.advisor_room_id().is_some()
                        && ctx.advisor_room_view.area.width > 0
                        && ctx.advisor_room_view.area.height > 0
                    {
                        ctx.advisor_room_view.area
                    } else if composer.is_research_room()
                        && ctx.research_room_view.area.width > 0
                        && ctx.research_room_view.area.height > 0
                    {
                        ctx.research_room_view.area
                    } else {
                        f.area()
                    };
                    render_composer(f, composer_area, composer);
                }

                if !ctx.entry_chrome_fade_complete {
                    apply_entry_chrome_fade(f);
                }

                ctx.click_regions = regions;
                ctx.left_pane_area = areas.left_content;
                ctx.supervisor_pane_area = areas.supervisor_area;
            })?;
            ctx.needs_redraw = false;
        } // if ctx.needs_redraw

        if ctx
            .pending_escape_sequence
            .as_ref()
            .is_some_and(|pending| pending.started_at.elapsed() >= RAW_ESCAPE_SEQUENCE_TIMEOUT)
        {
            flush_pending_escape_sequence(ctx);
        }

        // Handle input — bounded wait using active/idle tick rate.
        // The pre-drain call at the top of the loop body already drained
        // anything that was already pending before output processing
        // started; this one services events that arrived during the
        // poll_batch + render work, plus the case where the user wasn't
        // typing at all (we wait up to `tick_rate` here so the loop
        // doesn't spin). See § F8a in tmp/tick-latency/GOAL_PROMPT.md.
        let tick_rate = if ctx.last_output_at.elapsed() < ctx.idle_threshold || ctx.needs_redraw {
            ctx.tick_active
        } else {
            ctx.tick_idle
        };
        let post_drain = drain_pending_input(ctx, tick_rate, &focused_id, &active_left_id)?;
        if post_drain.should_break {
            break;
        }

        // Flush queued prompt delivery only after the first frame has rendered.
        // Gateway-backed prompts may block on process/session startup or prompt
        // acknowledgements, which should never prevent the TUI from showing an
        // initial frame.
        ctx.mux.flush_pending_startup_prompts(&ctx.rt);

        // Poll session registration files every 2 seconds for dashboard.
        if ctx.last_session_poll.elapsed() >= ctx.session_poll_interval {
            ctx.needs_redraw = true; // dashboard data may change
            for (pane_id, stale_tools, stale_operation, still_busy) in ctx
                .mux
                .sweep_stale_activity_locks(STALE_ACTIVE_TOOL_THRESHOLD)
            {
                tracing::warn!(
                    pane = %pane_id,
                    stale_tools = ?stale_tools,
                    stale_operation,
                    still_busy,
                    "Cleared stale runtime activity lock"
                );
                if !still_busy {
                    push_dashboard_event(
                        &ctx.dashboard_data,
                        format!(
                            "cleared stale activity lock for {pane_id}; idle recovery may recycle it"
                        ),
                    );
                }
            }

            let brehon_root = ctx.dashboard_data.lock().unwrap().brehon_root.clone();
            if let Some(ref root) = brehon_root {
                if ctx.pending_dashboard_refresh.is_none() {
                    let root = root.clone();
                    let session_entries = collect_session_refresh_entries(&ctx.mux);
                    let fallback_panels = ctx.fallback_panels.clone();
                    ctx.pending_dashboard_refresh = Some(ctx.rt.spawn_blocking(move || {
                        collect_dashboard_refresh(&root, &session_entries, &fallback_panels)
                    }));
                }
            }
            if let Some(ref root) = brehon_root {
                super::prompt_delivery::deliver_pending_prompts(ctx, root);
            }

            ctx.last_session_poll = std::time::Instant::now();
        }

        super::stall_handling::detect_and_handle_stalls(ctx);
    }

    Ok(())
}

pub(super) fn runtime_command_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_ports::RuntimeCommandRouter;
    use brehon_types::config::WorkerIdleBehavior;
    use ratatui::{TerminalOptions, Viewport};
    use std::sync::mpsc;

    struct RecordedRoute {
        command: RuntimeCommand,
        context: RuntimePolicyContext,
    }

    struct RecordingRouter {
        tx: std::sync::Mutex<mpsc::Sender<RecordedRoute>>,
    }

    #[async_trait::async_trait]
    impl RuntimeCommandRouter for RecordingRouter {
        async fn route_command(
            &self,
            command: RuntimeCommand,
            context: RuntimePolicyContext,
        ) -> Result<brehon_types::RuntimeCommandResult, PortError> {
            let command_id = command.command_id.clone();
            self.tx
                .lock()
                .expect("router lock")
                .send(RecordedRoute { command, context })
                .expect("record route");
            Ok(brehon_types::RuntimeCommandResult {
                command_id,
                status: RuntimeCommandStatus::Applied,
                message: Some("recorded".to_string()),
            })
        }
    }

    struct TestHarness {
        _rt: tokio::runtime::Runtime,
        ctx: EventLoopCtx,
        rx: mpsc::Receiver<RecordedRoute>,
    }

    fn orchestration() -> OrchestrationConfig {
        OrchestrationConfig {
            max_active_workers: 1,
            worktree_isolation: true,
            branch_prefix: "brehon/".to_string(),
            auto_cleanup_worktrees: true,
            worker_idle_behavior: WorkerIdleBehavior::Wait,
            allow_mutating_idle_work: false,
            self_improve_tasks: Vec::new(),
            spawn_workers: None,
            drain_timeout_secs: None,
        }
    }

    fn make_worker_pane(name: &str) -> brehon_mux::Pane {
        let dir = tempfile::tempdir().expect("tempdir");
        brehon_mux::Pane::worker(
            name,
            dir.path().to_path_buf(),
            None,
            "supervisor",
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            None,
            None,
            24,
            80,
            None,
            None,
        )
        .expect("worker pane")
    }

    fn make_reviewer_pane(name: &str) -> brehon_mux::Pane {
        let dir = tempfile::tempdir().expect("tempdir");
        brehon_mux::Pane::reviewer_with_agent_type(
            name,
            dir.path().to_path_buf(),
            None,
            None,
            24,
            80,
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .expect("reviewer pane")
    }

    fn make_supervisor_pane(name: &str) -> brehon_mux::Pane {
        let dir = tempfile::tempdir().expect("tempdir");
        let adapter = brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Claude);
        brehon_mux::Pane::supervisor(
            name,
            dir.path().to_path_buf(),
            None,
            24,
            80,
            &adapter,
            &adapter,
            &[],
            None,
            None,
            None,
            &std::collections::HashMap::new(),
        )
        .expect("supervisor pane")
    }

    fn harness_with_mux(mux: Mux) -> TestHarness {
        harness_with_mux_and_host_owned(mux, false)
    }

    fn harness_with_host_owned_mux(mux: Mux) -> TestHarness {
        harness_with_mux_and_host_owned(mux, true)
    }

    fn harness_with_mux_and_host_owned(mux: Mux, host_owned: bool) -> TestHarness {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let rt_handle = rt.handle().clone();
        let (tx, rx) = mpsc::channel();
        let router: Arc<dyn RuntimeCommandRouter> = Arc::new(RecordingRouter {
            tx: std::sync::Mutex::new(tx),
        });
        let now = Instant::now();
        let worker_ids = mux
            .panes()
            .filter(|pane| pane.kind() == &PaneKind::Worker)
            .map(|pane| pane.id().to_string())
            .collect::<Vec<_>>();
        let all_reviewer_ids = mux
            .panes()
            .filter(|pane| pane.kind() == &PaneKind::Reviewer)
            .map(|pane| pane.id().to_string())
            .collect::<Vec<_>>();
        let advisor_ids = mux
            .panes()
            .filter(|pane| pane.kind() == &PaneKind::Advisor)
            .map(|pane| pane.id().to_string())
            .collect::<Vec<_>>();
        let research_ids = mux
            .panes()
            .filter(|pane| pane.kind() == &PaneKind::Research)
            .map(|pane| pane.id().to_string())
            .collect::<Vec<_>>();
        let supervisor_id = mux
            .panes()
            .find(|pane| pane.kind() == &PaneKind::Supervisor)
            .map(|pane| pane.id().to_string());
        let last_activity = mux
            .panes()
            .map(|pane| (pane.id().to_string(), now))
            .collect::<std::collections::HashMap<_, _>>();
        let terminal = Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("fixed terminal");
        let dashboard_data = Arc::new(std::sync::Mutex::new(DashboardData::default()));
        let runtime_session_name = mux.session_name().map(str::to_string);

        TestHarness {
            _rt: rt,
            ctx: EventLoopCtx {
                shutdown: Arc::new(AtomicBool::new(false)),
                mux,
                runtime_command_rx: None,
                runtime_event_rx: None,
                runtime_command_router: Some(router),
                rt: rt_handle,
                terminal,
                dashboard_data,
                orchestration: orchestration(),
                tick_active: Duration::from_millis(16),
                tick_idle: Duration::from_millis(100),
                idle_threshold: Duration::from_secs(1),
                last_output_at: now,
                started_at: now,
                group_tab: GroupTab::Workers,
                prev_group_tab: GroupTab::Workers,
                click_regions: Vec::new(),
                selection: None,
                pending_down: None,
                pending_escape_sequence: None,
                left_pane_area: Rect::default(),
                supervisor_pane_area: Rect::default(),
                expanded_epics: std::collections::HashSet::new(),
                expanded_activity_rows: std::collections::HashSet::new(),
                structured_scroll_offsets: std::collections::HashMap::new(),
                input_mode: InputMode::default(),
                task_detail: None,
                advisor_room_view: AdvisorRoomViewState::default(),
                research_room_view: ResearchRoomViewState::default(),
                dashboard_agent_list: DashboardAgentListState::default(),
                dashboard_task_list: DashboardTaskListState::default(),
                structured_mode: std::collections::HashSet::new(),
                last_activity,
                auto_recover_threshold: Duration::from_secs(60),
                worker_context_reset_cooldown: Duration::from_secs(60),
                self_improve_idle_threshold: Duration::from_secs(60),
                self_improve_retry_cooldown: Duration::from_secs(60),
                last_stall_check: now - Duration::from_secs(60),
                stall_check_interval: Duration::from_secs(60),
                supervisor_dispatch_nudge_quiet_threshold: Duration::from_secs(60),
                supervisor_dispatch_nudge_cooldown: Duration::from_secs(60),
                last_supervisor_dispatch_nudge: None,
                last_supervisor_reset: std::collections::HashMap::new(),
                last_worker_context_reset: std::collections::HashMap::new(),
                pending_self_improve_prompt: std::collections::HashMap::new(),
                next_self_improve_index: std::collections::HashMap::new(),
                post_checkpoint_nudge_threshold: Duration::from_secs(60),
                post_checkpoint_nudge_cooldown: Duration::from_secs(60),
                post_checkpoint_nudges_sent: std::collections::HashMap::new(),
                review_obligation_nudges_sent: std::collections::HashMap::new(),
                active_worker_recovery_nudges_sent: std::collections::HashMap::new(),
                active_worker_recovery_resets_sent: std::collections::HashMap::new(),
                worker_ids,
                all_reviewer_ids,
                advisor_ids,
                research_ids,
                supervisor_id,
                fallback_panels: Vec::new(),
                has_panels: false,
                panels: Vec::new(),
                selected_worker: 0,
                selected_panel: 0,
                selected_member: Vec::new(),
                reviewer_selection: ReviewerSelectionState::default(),
                pending_initial_resize: false,
                last_session_poll: now,
                session_poll_interval: Duration::from_secs(60),
                runtime_session_name,
                last_shared_root_issue: None,
                pending_dashboard_refresh: None,
                pending_queued_gateway_prompt_deliveries: Vec::new(),
                pending_runtime_commands: Vec::new(),
                recent_runtime_commands: Vec::new(),
                pending_runtime_approval_resolutions: Vec::new(),
                entry_chrome_fade_complete: false,
                project_config_loader: crate::run::no_project_config_loader(),
                needs_redraw: false,
                runtime_agent_factory_host_owned: host_owned,
                runtime_terminal_host_absolute_resize: false,
            },
            rx,
        }
    }

    fn recv_route(rx: &mpsc::Receiver<RecordedRoute>) -> RecordedRoute {
        rx.recv_timeout(Duration::from_secs(1))
            .expect("daemon route command")
    }

    fn drain_runtime_commands(harness: &mut TestHarness) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !harness.ctx.pending_runtime_commands.is_empty() && Instant::now() < deadline {
            harness
                ._rt
                .block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
            process_pending_runtime_commands(&mut harness.ctx);
        }
    }

    fn write_review_obligation_fixture(brehon_root: &std::path::Path) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        let review_round_dir = runtime_dir.join("reviews").join("T-review").join("round-1");
        std::fs::create_dir_all(&review_round_dir).expect("review dir");
        std::fs::write(
            runtime_dir.join("tasks").join("T-review.json"),
            serde_json::json!({
                "task_id": "T-review",
                "title": "Pending review task",
                "status": "in_review",
                "task_type": "task"
            })
            .to_string(),
        )
        .expect("task file");
        std::fs::write(
            runtime_dir
                .join("reviews")
                .join("T-review")
                .join("state.json"),
            serde_json::json!({
                "task_id": "T-review",
                "status": "collecting",
                "current_round": 1,
                "current_review_id": "REV-review",
                "max_rounds": 3,
                "panel_id": "primary",
                "panel": ["reviewer-1"],
                "submissions_received": [],
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("review state");
        std::fs::write(
            review_round_dir.join("request.json"),
            serde_json::json!({
                "task_id": "T-review",
                "review_id": "REV-review",
                "requested_by": "supervisor",
                "requested_at": chrono::Utc::now().to_rfc3339(),
                "title": "Pending review task",
                "description": "Review the pending change",
                "commit": "abc1234",
                "base_commit": "def5678",
                "merge_target_head": "main",
                "commits": ["abc1234"],
                "context": "Focused context"
            })
            .to_string(),
        )
        .expect("review request");
    }

    fn write_active_assigned_task_fixture(brehon_root: &std::path::Path) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        std::fs::write(
            runtime_dir.join("tasks").join("T-owned.json"),
            serde_json::json!({
                "task_id": "T-owned",
                "title": "Owned task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1"
            })
            .to_string(),
        )
        .expect("task file");
    }

    #[test]
    fn host_owned_keyboard_input_routes_through_runtime_router() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(brehon_mux::Pane::director("pane-1", 24, 80).expect("director"));
        let mut harness = harness_with_host_owned_mux(mux);

        forward_input_bytes(&mut harness.ctx, b"hello\n");

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("pane-1"));
        assert_eq!(routed.command.target.generation, Some(0));
        assert!(routed.context.pane_state.is_none());
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendTerminalInput { ref bytes } if bytes == b"hello\n"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
        assert_eq!(harness.ctx.recent_runtime_commands.len(), 1);
        assert_eq!(
            harness.ctx.recent_runtime_commands[0].target.as_deref(),
            Some("pane-1")
        );
        assert_eq!(
            harness.ctx.recent_runtime_commands[0].label,
            "terminal-input"
        );
        assert_eq!(harness.ctx.recent_runtime_commands[0].status, "pending");

        drain_runtime_commands(&mut harness);

        assert!(harness.ctx.pending_runtime_commands.is_empty());
        assert_eq!(harness.ctx.recent_runtime_commands[0].status, "applied");
        assert_eq!(
            harness.ctx.recent_runtime_commands[0].message.as_deref(),
            Some("recorded")
        );
    }

    #[test]
    fn embedded_keyboard_input_bypasses_runtime_router() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(brehon_mux::Pane::director("pane-1", 24, 80).expect("director"));
        let mut harness = harness_with_mux(mux);

        forward_input_bytes(&mut harness.ctx, b"hello\n");

        assert!(harness.rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(harness.ctx.pending_runtime_commands.is_empty());
        assert!(harness.ctx.recent_runtime_commands.is_empty());
    }

    #[test]
    fn ctrl_f_attaches_only_focused_panesmith_supervisor() {
        let project_root = tempfile::tempdir().expect("tempdir");
        let mut mux = Mux::factory(brehon_mux::MuxConfig {
            cwd: project_root.path().to_path_buf(),
            workers: 0,
            supervisor_name: "codex-supervisor".to_string(),
            supervisor_cli: brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            include_director: false,
            rows: 24,
            cols: 100,
            ..Default::default()
        })
        .expect("create mux");
        mux.focus("codex-supervisor");
        let harness = harness_with_mux(mux);
        let ctrl_f = crossterm::event::KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        let raw_ctrl_f =
            crossterm::event::KeyEvent::new(KeyCode::Char('\u{6}'), KeyModifiers::empty());
        let raw_ctrl_f_with_modifier =
            crossterm::event::KeyEvent::new(KeyCode::Char('\u{6}'), KeyModifiers::CONTROL);

        assert!(should_attach_focused_panesmith_supervisor(
            &harness.ctx,
            &ctrl_f
        ));
        assert!(should_attach_focused_panesmith_supervisor(
            &harness.ctx,
            &raw_ctrl_f
        ));
        assert!(should_attach_focused_panesmith_supervisor(
            &harness.ctx,
            &raw_ctrl_f_with_modifier
        ));

        let mut ghostty_mux = Mux::new(24, 80);
        ghostty_mux.add_pane(make_supervisor_pane("claude-supervisor"));
        ghostty_mux.focus("claude-supervisor");
        let ghostty_harness = harness_with_mux(ghostty_mux);

        assert!(!should_attach_focused_panesmith_supervisor(
            &ghostty_harness.ctx,
            &ctrl_f
        ));
        assert!(!should_attach_focused_panesmith_supervisor(
            &ghostty_harness.ctx,
            &raw_ctrl_f
        ));
        assert!(!should_attach_focused_panesmith_supervisor(
            &ghostty_harness.ctx,
            &raw_ctrl_f_with_modifier
        ));
    }

    #[test]
    fn panesmith_dashboard_attach_reuses_brehon_prepared_screen() {
        let options = panesmith_attach_options_for_dashboard();

        assert_eq!(
            options.screen,
            panesmith::AttachScreenPolicy::ReuseHostAlternateScreen
        );
        assert_eq!(options.detach.chord, vec![0x06]);
    }

    #[test]
    fn manual_reset_routes_through_runtime_router() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("supervisor"));
        let mut harness = harness_with_mux(mux);

        assert!(perform_manual_reset_request(&mut harness.ctx, "supervisor"));

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("supervisor"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason } if reason == "manual reset"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
    }

    #[test]
    fn stall_recycle_routes_through_runtime_router() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now - Duration::from_secs(5));

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-recover idle worker pane via daemon recycle"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
    }

    #[test]
    fn dead_worker_recycle_routes_without_idle_threshold() {
        let mut mux = Mux::new(24, 80);
        let mut pane = make_worker_pane("worker-1");
        pane.mark_exited(Some(1));
        mux.add_pane(pane);
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60 * 60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now);

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-recover dead worker pane via daemon recycle"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
    }

    #[test]
    fn stale_active_assigned_worker_gets_nudged_before_reset() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now - Duration::from_secs(5));
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("Worker liveness nudge")
                    && text.contains("T-owned")
                    && text.contains("action=mine")
        ));
        assert!(harness
            .ctx
            .active_worker_recovery_nudges_sent
            .contains_key(&("worker-1".to_string(), "T-owned".to_string())));
    }

    #[test]
    fn stale_active_assigned_worker_resets_once_after_nudge_window() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.active_worker_recovery_nudges_sent.insert(
            ("worker-1".to_string(), "T-owned".to_string()),
            now - Duration::from_secs(5),
        );
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now - Duration::from_secs(5));
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason }
                if reason == "auto-recover idle assigned worker pane via daemon reset"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);

        drain_runtime_commands(&mut harness);
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
        assert!(harness
            .ctx
            .active_worker_recovery_resets_sent
            .contains_key(&("worker-1".to_string(), "T-owned".to_string())));

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.last_activity.insert(
            "worker-1".to_string(),
            Instant::now() - Duration::from_secs(5),
        );
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "active assigned worker reset must not loop for the same task"
        );
    }

    #[test]
    fn busy_active_assigned_worker_is_not_nudged_or_reset() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let generation = mux
            .get("worker-1")
            .expect("worker pane")
            .current_generation();
        mux.mark_gateway_delivery_busy(
            "worker-1",
            brehon_types::PromptId::new("busy-prompt"),
            generation,
        );
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now - Duration::from_secs(5));
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "busy active worker should not be nudged or reset"
        );
        assert!(harness.ctx.pending_runtime_commands.is_empty());
    }

    #[test]
    fn stale_deferred_prompt_delivery_recycles_busy_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let prompt_path = temp.path().join("worker-1.prompt");
        std::fs::write(&prompt_path, "continue").expect("write prompt");
        let retry_meta = crate::run::recovery::prompt_retry_meta_path(&prompt_path);
        let first_deferred_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        std::fs::write(
            retry_meta,
            serde_json::to_string(&serde_json::json!({
                "attempts": 0,
                "deferrals": 3,
                "first_deferred_at": first_deferred_at.to_rfc3339(),
                "last_deferred_at": chrono::Utc::now().to_rfc3339(),
                "last_deferred_reason": "daemon prompt delivery deferred",
                "next_retry_at": chrono::Utc::now().to_rfc3339(),
            }))
            .expect("retry json"),
        )
        .expect("write retry meta");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let generation = mux
            .get("worker-1")
            .expect("worker pane")
            .current_generation();
        mux.mark_gateway_delivery_busy(
            "worker-1",
            brehon_types::PromptId::new("busy-prompt"),
            generation,
        );
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now - Duration::from_secs(120));

        assert!(
            crate::run::stall_handling::recover_stale_deferred_prompt_delivery(
                &mut harness.ctx,
                "worker-1",
                &prompt_path,
                now,
            )
        );

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-recover worker after stale queued prompt delivery via daemon recycle"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
    }

    #[test]
    fn stale_reviewer_obligation_gets_nudged_before_reset() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("reviewer-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("Review-obligation nudge")
                    && text.contains("action=review_status task_id=T-review review_id=REV-review")
        ));
        assert!(harness.ctx.review_obligation_nudges_sent.contains_key(&(
            "reviewer-1".to_string(),
            "T-review".to_string(),
            "REV-review".to_string()
        )));
    }

    #[test]
    fn stale_reviewer_obligation_resets_once_after_nudge_window() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.review_obligation_nudges_sent.insert(
            (
                "reviewer-1".to_string(),
                "T-review".to_string(),
                "REV-review".to_string(),
            ),
            now - Duration::from_secs(5),
        );
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("reviewer-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason }
                if reason == "auto-recover idle reviewer pane with pending review obligation"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);

        drain_runtime_commands(&mut harness);
        let ack_path = brehon_root
            .join("runtime")
            .join("reviewer-reset-acks")
            .join("T-review--REV-review--reviewer-1.json");
        assert!(
            ack_path.exists(),
            "successful reset writes a per-review ack"
        );

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.last_activity.insert(
            "reviewer-1".to_string(),
            Instant::now() - Duration::from_secs(5),
        );
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "acknowledged reviewer obligation reset must not loop"
        );
    }

    #[test]
    fn quarantined_worker_with_active_task_routes_context_reset_and_clears_health() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        std::fs::create_dir_all(runtime_dir.join("sessions")).expect("sessions dir");
        std::fs::create_dir_all(runtime_dir.join("agent-health")).expect("health dir");

        std::fs::write(
            runtime_dir.join("tasks").join("T-owned.json"),
            serde_json::json!({
                "task_id": "T-owned",
                "title": "Owned task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1"
            })
            .to_string(),
        )
        .expect("task file");
        std::fs::write(
            runtime_dir.join("sessions").join("worker-1.json"),
            serde_json::json!({
                "name": "worker-1",
                "role": "worker",
                "session_id": "session-1",
                "last_seen_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("session file");
        let health_path = runtime_dir.join("agent-health").join("worker-1.json");
        std::fs::write(
            &health_path,
            serde_json::json!({
                "agent": "worker-1",
                "status": "unavailable",
                "reason": "non_retryable_http_status"
            })
            .to_string(),
        )
        .expect("health file");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason }
                if reason == "auto-recover quarantined worker pane via daemon reset"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);

        drain_runtime_commands(&mut harness);

        assert!(
            !health_path.exists(),
            "successful reset should clear stale quarantine marker"
        );
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn durable_prompt_delivery_routes_through_runtime_router_and_acks_on_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let queue_dir = brehon_root.join("runtime").join("prompt-queue");
        std::fs::create_dir_all(&queue_dir).expect("queue dir");
        let prompt_path = queue_dir.join("000.prompt");
        std::fs::write(
            &prompt_path,
            serde_json::json!({
                "target": "worker-1",
                "from": "supervisor",
                "message": "please continue",
                "prompt_id": "prompt-1"
            })
            .to_string(),
        )
        .expect("prompt file");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        crate::run::prompt_delivery::deliver_pending_prompts(&mut harness.ctx, &brehon_root);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt {
                ref text,
                ref from,
                delivery: brehon_types::PromptDeliveryMode::Attempt,
                ..
            } if text == "please continue" && from.as_deref() == Some("supervisor")
        ));

        let deadline = Instant::now() + Duration::from_secs(1);
        while !harness.ctx.pending_runtime_commands.is_empty() && Instant::now() < deadline {
            harness
                ._rt
                .block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
            process_pending_runtime_commands(&mut harness.ctx);
        }

        assert!(harness.ctx.pending_runtime_commands.is_empty());
        assert!(
            !prompt_path.exists(),
            "successful daemon delivery removes prompt file"
        );
        assert!(
            brehon_root
                .join("runtime")
                .join("prompt-delivery-acks")
                .join("prompt-1.json")
                .exists(),
            "successful daemon delivery writes prompt ack"
        );
    }

    #[test]
    fn composer_submission_routes_through_durable_prompt_queue() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("supervisor"));
        let mut harness = harness_with_host_owned_mux(mux);
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        let mut state = ComposerState::new("supervisor", None);
        state.workflow = super::super::types::ComposerWorkflow::BreakDown;
        state.text = "split the accepted design into executable tasks".to_string();
        let message = super::super::composer::build_composer_message(&state);
        submit_composer_message(
            &mut harness.ctx,
            ComposerSubmission {
                state: state.clone(),
                message,
            },
        );

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("supervisor"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt {
                ref text,
                ref from,
                delivery: brehon_types::PromptDeliveryMode::Direct,
                ..
            } if text.contains("Skill: brehon-breakdown")
                && text.contains("split the accepted design")
                && from.as_deref() == Some("operator")
        ));
        assert!(harness.ctx.pending_runtime_commands.is_empty());

        let queue_dir = brehon_root
            .join("runtime")
            .join("prompt-queue")
            .join("_legacy");
        let remaining_entries = std::fs::read_dir(&queue_dir)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(remaining_entries, 0);
        let ack_count = std::fs::read_dir(brehon_root.join("runtime").join("prompt-delivery-acks"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(ack_count, 1);
    }

    #[test]
    fn advisor_worker_mention_routes_durable_prompt_to_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("quick-cod-72"));
        let mut harness = harness_with_host_owned_mux(mux);
        harness
            .ctx
            .dashboard_data
            .lock()
            .expect("dashboard")
            .brehon_root = Some(brehon_root.clone());

        let mut state = ComposerState::new_advisor("norma-war-room")
            .with_mention_candidates(vec!["quick-cod-72".to_string()]);
        state.text = "@quick-cod-72 can you sanity check the rollback concern?".to_string();
        let message = super::super::composer::build_composer_message(&state);
        submit_composer_message(
            &mut harness.ctx,
            ComposerSubmission {
                state: state.clone(),
                message,
            },
        );

        let routed = recv_route(&harness.rx);
        assert_eq!(
            routed.command.target.pane_id.as_deref(),
            Some("quick-cod-72")
        );
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt {
                ref text,
                ref from,
                delivery: brehon_types::PromptDeliveryMode::Direct,
                ..
            } if text.contains("Operator mentioned you in advisor room norma-war-room")
                && text.contains("do not change task ownership")
                && from.as_deref() == Some("operator")
        ));

        let room_path = brehon_root
            .join("runtime")
            .join("advisors")
            .join("rooms")
            .join("norma-war-room.json");
        let room = std::fs::read_to_string(room_path).expect("room file");
        assert!(room.contains("@quick-cod-72 can you sanity check"));
    }
}
