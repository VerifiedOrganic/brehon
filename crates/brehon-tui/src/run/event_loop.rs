//! Event loop extracted from `run_tui_with_panels`.
//!
//! The `EventLoopCtx` struct bundles all mutable state that lives across loop
//! iterations.  `run()` contains the `while !shutdown` loop body; setup and
//! teardown stay in `mod.rs`.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use brehon_mux::{
    Mux, MuxEvent, MuxRuntimeCommandReceiver, PaneKind, PromptDeliveryAttempt, SessionScopedQueue,
};
use brehon_ports::{PortError, RuntimeCommandRouter};
use brehon_types::config::OrchestrationConfig;
use brehon_types::{
    RuntimeCommand, RuntimeCommandKind, RuntimeCommandStatus, RuntimeCommandTarget, RuntimeEvent,
    RuntimePaneBlockInfo, RuntimePaneKind, RuntimePaneState, RuntimePolicyContext,
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
    block_task_for_prompt_block_recovery_failure, clear_agent_health_marker,
    clear_prompt_retry_meta, dead_letter_prompt_for_session, promote_active_assigned_task,
    prompt_blocked_detail, prompt_blocked_info, push_dashboard_event,
    queued_prompt_backpressure_retry_delay, record_prompt_retry_deferral,
    record_prompt_retry_failure, should_dead_letter_prompt_after_failure,
    sync_worker_task_contexts, write_prompt_blocked_recovery_failed_marker_or_clear_stale_marker,
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
use super::terminal_guard::BrehonDashboardTerminalControl;
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
    pub dashboard_data: Arc<parking_lot::Mutex<DashboardData>>,
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
    /// Time elapsed after the last *nudge* before a reviewer is eligible for
    /// escalation. Resends are tracked separately and do not start this timer.
    pub review_obligation_nudge_threshold: Duration,
    pub review_obligation_reset_threshold: Duration,
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
    pub prompt_blocked_recovery_failed_panes: std::collections::HashSet<String>,

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
    pub review_obligation_notifications_sent:
        std::collections::HashMap<(String, String, String), Instant>,
    pub review_obligation_resends_sent:
        std::collections::HashMap<(String, String, String), Instant>,
    pub review_obligation_failures_reported: std::collections::HashSet<(String, String, String)>,
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
    pub last_panesmith_snapshot_panes: BTreeSet<String>,
    pub force_panesmith_snapshot_refresh: bool,

    /// Loads the merged project `BrehonConfig` on demand. Injected from
    /// `brehon-cli` so this crate can stay free of a `brehon-config` dep.
    pub project_config_loader: super::research::ProjectConfigLoader,

    // Budget kill-switch state (see `run::budget`).
    /// Last time the budget gate was evaluated (self-throttle).
    pub last_budget_check: Instant,
    /// How often the budget gate re-reads spend and re-evaluates.
    pub budget_check_interval: Duration,
    /// One-shot latch: true once budget teardown has fired, so the inline
    /// `mux.shutdown_all()` and the breach signal never repeat per tick.
    pub budget_torn_down: bool,
    /// Cached refusal reason set by `budget_tick`; read by the dispatch seam so
    /// the hot path fails closed without a per-prompt filesystem read.
    pub budget_block_dispatch: Option<String>,
    /// Dedupe key for the last Soft budget warning pushed to the dashboard.
    pub last_budget_warn: Option<String>,
    /// Injected durable sink for budget-breach signals (optional). Avoids a new
    /// exhaustively-matched `EventKind` variant; when `None` the operator signal
    /// degrades to a dashboard event + `tracing::warn!`.
    pub budget_event_sink: Option<super::budget::BudgetEventSink>,

    pub needs_redraw: bool,
}

pub(crate) struct PendingRuntimeCommandTask {
    command_id: String,
    effect: PendingRuntimeCommandEffect,
    handle: tokio::task::JoinHandle<Result<brehon_types::RuntimeCommandResult, PortError>>,
}

fn active_left_pane_id(ctx: &EventLoopCtx) -> Option<String> {
    match ctx.group_tab {
        GroupTab::Dashboard | GroupTab::Runtime | GroupTab::Advisors | GroupTab::Research => None,
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
    }
}

fn visible_panesmith_snapshot_panes(
    ctx: &EventLoopCtx,
    active_left_id: Option<&str>,
) -> BTreeSet<String> {
    let mut panes = BTreeSet::new();
    if let Some(pane_id) = active_left_id {
        if ctx.mux.is_panesmith_managed(pane_id) {
            panes.insert(pane_id.to_string());
        }
    }
    if !ctx.runtime_agent_factory_host_owned {
        if let Some(supervisor_id) = ctx.supervisor_id.as_deref() {
            if ctx.mux.is_panesmith_managed(supervisor_id) {
                panes.insert(supervisor_id.to_string());
            }
        }
    }
    if let Some(focused_id) = ctx.mux.focused_id() {
        if ctx.mux.is_panesmith_managed(focused_id) {
            panes.insert(focused_id.to_string());
        }
    }
    panes
}

fn panesmith_snapshot_refresh_targets(
    current: &BTreeSet<String>,
    previous: &BTreeSet<String>,
    force_refresh: bool,
) -> BTreeSet<String> {
    if force_refresh {
        return current.clone();
    }
    current.difference(previous).cloned().collect()
}

fn pane_is_visible_for_output(
    pane_id: &str,
    active_left_id: Option<&str>,
    supervisor_id: Option<&str>,
    runtime_agent_factory_host_owned: bool,
) -> bool {
    active_left_id == Some(pane_id)
        || (!runtime_agent_factory_host_owned && supervisor_id == Some(pane_id))
}

fn mux_event_affects_visible_ui(
    event: &MuxEvent,
    active_left_id: Option<&str>,
    supervisor_id: Option<&str>,
    runtime_agent_factory_host_owned: bool,
) -> bool {
    match event {
        MuxEvent::PaneOutput { pane_id, .. }
        | MuxEvent::ActivityEvent { pane_id, .. }
        | MuxEvent::ActivityFlush { pane_id, .. }
        | MuxEvent::TaskContextChanged { pane_id, .. }
        | MuxEvent::ReviewContextChanged { pane_id, .. } => pane_is_visible_for_output(
            pane_id,
            active_left_id,
            supervisor_id,
            runtime_agent_factory_host_owned,
        ),
        MuxEvent::PaneExited { .. }
        | MuxEvent::PaneAdded { .. }
        | MuxEvent::PaneRemoved { .. }
        | MuxEvent::FocusChanged { .. }
        | MuxEvent::AsyncGatewayPromptDeliveryCompleted { .. }
        | MuxEvent::AsyncTeamsPromptDeliveryCompleted { .. } => true,
    }
}

fn visible_structured_pane_has_active_tool(
    ctx: &EventLoopCtx,
    active_left_id: Option<&str>,
    supervisor_id: Option<&str>,
) -> bool {
    for id in [active_left_id, supervisor_id].into_iter().flatten() {
        if !ctx.structured_mode.contains(id) {
            continue;
        }
        let Some(pane) = ctx.mux.get(id) else {
            continue;
        };
        let Some(buf) = pane.activity_buffer() else {
            continue;
        };
        if buf.active_tools().next().is_some() {
            return true;
        }
    }
    false
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
    WorkerRecycle {
        pane_id: String,
        owning_task_id: Option<String>,
        blocked: Option<RuntimePaneBlockInfo>,
        startup_prompt: Option<String>,
        success_message: String,
        failure_prefix: String,
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
            ctx.runtime_agent_factory_host_owned,
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
                build_supervisor_reset_startup_prompt(
                    &ctx.mux,
                    pane_id,
                    ctx.runtime_agent_factory_host_owned,
                )
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

pub(super) fn worker_recycle_command_pending(ctx: &EventLoopCtx, pane_id: &str) -> bool {
    ctx.pending_runtime_commands.iter().any(|task| {
        matches!(
            &task.effect,
            PendingRuntimeCommandEffect::WorkerRecycle {
                pane_id: pending_pane_id,
                ..
            } if pending_pane_id == pane_id
        )
    })
}

pub(super) fn worker_reset_or_recycle_command_pending(ctx: &EventLoopCtx, pane_id: &str) -> bool {
    ctx.pending_runtime_commands.iter().any(|task| {
        matches!(
            &task.effect,
            PendingRuntimeCommandEffect::ManualReset {
                pane_id: pending_pane_id,
                ..
            }
            | PendingRuntimeCommandEffect::RecoveryReset {
                pane_id: pending_pane_id,
                ..
            }
            | PendingRuntimeCommandEffect::WorkerRecycle {
                pane_id: pending_pane_id,
                ..
            } if pending_pane_id == pane_id
        ) || matches!(
            &task.effect,
            PendingRuntimeCommandEffect::QueuedWorkerRecycle { request, .. }
                if request.worker == pane_id
        )
    })
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

fn flush_forward_input_buffer(ctx: &mut EventLoopCtx, forward_buf: &mut Vec<u8>) {
    if !forward_buf.is_empty() {
        forward_input_bytes(ctx, forward_buf);
        forward_buf.clear();
    }
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

fn should_attach_focused_panesmith_pane(
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
}

fn panesmith_attach_options_for_dashboard() -> panesmith::AttachOptions {
    let mut options = panesmith::AttachOptions::default();
    options.detach.chord = vec![0x06]; // Ctrl-f toggles fullscreen attach off.
    options.screen = panesmith::AttachScreenPolicy::ReuseHostAlternateScreen;
    options
}

#[cfg(unix)]
fn attach_focused_panesmith_pane(ctx: &mut EventLoopCtx) -> io::Result<()> {
    let Some(pane_id) = ctx.mux.focused_id().map(str::to_string) else {
        return Ok(());
    };

    ctx.selection = None;
    ctx.pending_down = None;
    ctx.structured_scroll_offsets.remove(&pane_id);
    ctx.click_regions.clear();
    ctx.terminal.backend_mut().flush()?;

    let mut terminal = panesmith::StdioAttachTerminal::new(io::stdout())?;
    let mut control = BrehonDashboardTerminalControl::new(io::stdout());
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
                format!("detached fullscreen pane {pane_id}: {:?}", outcome.reason),
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
fn attach_focused_panesmith_pane(ctx: &mut EventLoopCtx) -> io::Result<()> {
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
        let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
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
        let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
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
        let Some(brehon_root) = ctx.dashboard_data.lock().brehon_root.clone() else {
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
        let Some(brehon_root) = ctx.dashboard_data.lock().brehon_root.clone() else {
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

    let Some(brehon_root) = ctx.dashboard_data.lock().brehon_root.clone() else {
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

fn handle_global_control_key(
    ctx: &mut EventLoopCtx,
    key: &KeyEvent,
    forward_buf: &mut Vec<u8>,
) -> bool {
    if key.code == KeyCode::BackTab {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.click_regions.clear();
        if ctx.runtime_agent_factory_host_owned {
            switch_external_terminal_tab_relative(ctx, false);
            return true;
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
        return true;
    }

    if is_ctrl_char_key(key, ']') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.click_regions.clear();
        if ctx.runtime_agent_factory_host_owned {
            switch_external_terminal_tab_relative(ctx, true);
            return true;
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
        return true;
    }

    if is_ctrl_char_key(key, 'd') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.group_tab = GroupTab::Dashboard;
        ctx.click_regions.clear();
        return true;
    }

    if is_ctrl_char_key(key, 't') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.group_tab = GroupTab::Runtime;
        ctx.click_regions.clear();
        return true;
    }

    if is_ctrl_char_key(key, 'a') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.group_tab = GroupTab::Advisors;
        ctx.click_regions.clear();
        return true;
    }

    if is_ctrl_char_key(key, 'y') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.group_tab = GroupTab::Research;
        ctx.click_regions.clear();
        return true;
    }

    if is_ctrl_char_key(key, 'v') {
        if let Some(focused_id) = ctx.mux.focused_id().map(str::to_string) {
            if let Some(pane) = ctx.mux.get(&focused_id) {
                if pane.is_gateway_backed() {
                    if ctx.structured_mode.contains(&focused_id) {
                        ctx.structured_mode.remove(&focused_id);
                    } else {
                        ctx.structured_mode.insert(focused_id);
                    }
                    ctx.click_regions.clear();
                }
            }
        }
        return true;
    }

    if is_ctrl_char_key(key, 'w') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.click_regions.clear();
        if ctx.runtime_agent_factory_host_owned {
            switch_external_terminal_tab(ctx, "Workers");
            return true;
        }
        ctx.group_tab = GroupTab::Workers;
        if let Some(id) = ctx.worker_ids.get(ctx.selected_worker) {
            ctx.mux.focus(id);
        }
        return true;
    }

    if is_ctrl_char_key(key, 'e') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.click_regions.clear();
        if ctx.runtime_agent_factory_host_owned {
            switch_external_terminal_tab(ctx, "Reviewers");
            return true;
        }
        ctx.group_tab = GroupTab::Reviewers;
        focus_current_reviewer(
            &mut ctx.mux,
            &ctx.panels,
            ctx.selected_panel,
            &ctx.selected_member,
        );
        return true;
    }

    if is_ctrl_char_key(key, 's') {
        flush_forward_input_buffer(ctx, forward_buf);
        ctx.selection = None;
        ctx.task_detail = None;
        ctx.click_regions.clear();
        if ctx.runtime_agent_factory_host_owned {
            switch_external_terminal_tab(ctx, "Supervisor");
            return true;
        }
        if let Some(ref sup_id) = ctx.supervisor_id {
            ctx.mux.focus(sup_id);
        }
        return true;
    }

    if is_ctrl_char_key(key, 'r') {
        flush_forward_input_buffer(ctx, forward_buf);
        if let Some(focused_id) = ctx.mux.focused_id().map(str::to_string) {
            if perform_manual_reset_request(ctx, &focused_id) {
                ctx.needs_redraw = true;
            }
        }
        return true;
    }

    false
}

pub(super) fn process_pending_runtime_commands(ctx: &mut EventLoopCtx) {
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

pub(super) fn drain_runtime_command_receiver(ctx: &mut EventLoopCtx) {
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
}

pub(super) fn process_pending_runtime_approval_resolutions(ctx: &mut EventLoopCtx) {
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
                    tracing::warn!(error = %err, "Runtime approval resolution task failed");
                }
            }
            ctx.needs_redraw = true;
        } else {
            approval_resolution_index += 1;
        }
    }
}

pub(super) fn process_pending_queued_gateway_prompt_deliveries(ctx: &mut EventLoopCtx) {
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
                        ctx.dashboard_data.lock().brehon_root.clone(),
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
                            Instant::now(),
                        );
                    }
                    let retry_after = queued_prompt_backpressure_retry_delay(&task.path, ahead_of);
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
                    let retry_after = queued_prompt_backpressure_retry_delay(
                        &task.path,
                        position.retry_ahead_of(),
                    );
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
                    let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
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
                    let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
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
                        error: "watchdog aborted stuck queued gateway prompt delivery".to_string(),
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
            let retry_after = queued_prompt_backpressure_retry_delay(&path, 1);
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
        PendingRuntimeCommandEffect::WorkerRecycle {
            pane_id,
            startup_prompt,
            success_message,
            ..
        } => {
            ctx.mux.clear_pane_task_context(&pane_id);
            apply_runtime_reset_success(ctx, &pane_id, startup_prompt, &success_message, None);
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

fn prompt_blocked_failure_message(base: String, blocked: Option<&RuntimePaneBlockInfo>) -> String {
    let Some(blocked) = blocked else {
        return base;
    };
    format!(
        "{base}; blocked request context: {}",
        prompt_blocked_detail(blocked)
    )
}

fn persist_taskless_prompt_blocked_recovery_failure(
    ctx: &mut EventLoopCtx,
    brehon_root: &std::path::Path,
    pane_id: &str,
    failure: &str,
    blocked: Option<&RuntimePaneBlockInfo>,
) {
    let blocked = blocked
        .cloned()
        .or_else(|| prompt_blocked_info(brehon_root, pane_id, ctx.mux.get(pane_id)));
    if let Err(marker_err) = write_prompt_blocked_recovery_failed_marker_or_clear_stale_marker(
        brehon_root,
        pane_id,
        failure,
        blocked.as_ref(),
    ) {
        tracing::warn!(
            pane = %pane_id,
            error = %marker_err,
            "Failed to persist prompt-blocked terminal recovery failure marker"
        );
        ctx.prompt_blocked_recovery_failed_panes
            .insert(pane_id.to_string());
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
        PendingRuntimeCommandEffect::WorkerRecycle {
            pane_id,
            owning_task_id,
            blocked,
            failure_prefix,
            ..
        } => {
            let message = prompt_blocked_failure_message(
                format!("{failure_prefix}: {detail}"),
                blocked.as_ref(),
            );
            push_dashboard_event(&ctx.dashboard_data, message.clone());
            tracing::warn!(pane = %pane_id, error = %detail, "{message}");
            let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
            if let Some(brehon_root) = brehon_root {
                if let Some(task_id) = owning_task_id {
                    let blocked_task_result = block_task_for_prompt_block_recovery_failure(
                        &brehon_root,
                        &task_id,
                        &pane_id,
                        &message,
                    );
                    match blocked_task_result {
                        Ok(()) => {
                            clear_agent_health_marker(&brehon_root, &pane_id);
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "blocked task {task_id} after prompt-blocked idle worker {pane_id} could not be recycled"
                                ),
                            );
                        }
                        Err(task_err) => {
                            tracing::warn!(
                                pane = %pane_id,
                                task_id,
                                error = %task_err,
                                "Failed to mark prompt-blocked task as blocked after queued recycle failure"
                            );
                            let operator_failure = format!(
                                "{message}; could not mark task {task_id} blocked: {task_err}"
                            );
                            if let Err(marker_err) =
                                write_prompt_blocked_recovery_failed_marker_or_clear_stale_marker(
                                    &brehon_root,
                                    &pane_id,
                                    &operator_failure,
                                    blocked.as_ref(),
                                )
                            {
                                tracing::warn!(
                                    pane = %pane_id,
                                    task_id,
                                    error = %marker_err,
                                    "Failed to persist prompt-blocked terminal recovery failure marker after queued recycle task update failed"
                                );
                                ctx.prompt_blocked_recovery_failed_panes
                                    .insert(pane_id.clone());
                            }
                            push_dashboard_event(&ctx.dashboard_data, operator_failure);
                        }
                    }
                } else {
                    persist_taskless_prompt_blocked_recovery_failure(
                        ctx,
                        &brehon_root,
                        &pane_id,
                        &message,
                        blocked.as_ref(),
                    );
                }
            }
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
    let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
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
                brehon_mux::PaneState::Blocked { .. } => RuntimePaneState::Blocked,
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
    ctx.force_panesmith_snapshot_refresh = true;

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
    _focused_id: &Option<String>,
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

                if should_attach_focused_panesmith_pane(ctx, &key) {
                    if !forward_buf.is_empty() {
                        forward_input_bytes(ctx, &forward_buf);
                        forward_buf.clear();
                    }
                    attach_focused_panesmith_pane(ctx)?;
                    continue;
                }

                if handle_global_control_key(ctx, &key, &mut forward_buf) {
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

pub(super) fn detect_and_handle_supervisor_resets(
    ctx: &mut EventLoopCtx,
    batch_events: &[MuxEvent],
    loop_now: Instant,
) {
    let supervisor_reset_cooldown = Duration::from_secs(60);
    let mut supervisor_reset_candidates = std::collections::BTreeMap::new();
    for ev in batch_events {
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
            let Some(startup_prompt) = build_supervisor_reset_startup_prompt(
                &ctx.mux,
                &pane_id,
                ctx.runtime_agent_factory_host_owned,
            ) else {
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
}

pub(super) fn new_headless_event_loop_ctx(
    mux: Mux,
    rt: tokio::runtime::Handle,
    runtime_command_rx: Option<MuxRuntimeCommandReceiver>,
    runtime_command_router: Option<Arc<dyn RuntimeCommandRouter>>,
    runtime_agent_factory_host_owned: bool,
) -> io::Result<EventLoopCtx> {
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
    )?;
    let dashboard_data = Arc::new(parking_lot::Mutex::new(DashboardData::default()));
    let runtime_session_name = mux.session_name().map(str::to_string);

    let structured_mode = mux
        .panes()
        .filter(|pane| pane.is_gateway_backed())
        .map(|pane| pane.id().to_string())
        .collect::<std::collections::HashSet<_>>();

    Ok(EventLoopCtx {
        shutdown: Arc::new(AtomicBool::new(false)),
        mux,
        runtime_command_rx,
        runtime_event_rx: None,
        runtime_command_router,
        rt,
        terminal,
        dashboard_data,
        orchestration: OrchestrationConfig {
            max_active_workers: 1,
            worktree_isolation: true,
            branch_prefix: "brehon/".to_string(),
            auto_cleanup_worktrees: true,
            worker_idle_behavior: brehon_types::config::WorkerIdleBehavior::Wait,
            allow_mutating_idle_work: false,
            self_improve_tasks: Vec::new(),
            spawn_workers: None,
            drain_timeout_secs: None,
            worktree_root: None,
            cargo_target_root: None,
            worktree_cleanup: brehon_types::WorktreeCleanupConfig::default(),
        },
        tick_active: Duration::from_millis(50),
        tick_idle: Duration::from_millis(200),
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
        structured_mode,
        last_activity,
        auto_recover_threshold: Duration::from_secs(60),
        review_obligation_nudge_threshold: Duration::from_secs(60),
        review_obligation_reset_threshold: Duration::from_secs(120),
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
        prompt_blocked_recovery_failed_panes: std::collections::HashSet::new(),
        post_checkpoint_nudge_threshold: Duration::from_secs(60),
        post_checkpoint_nudge_cooldown: Duration::from_secs(60),
        post_checkpoint_nudges_sent: std::collections::HashMap::new(),
        review_obligation_notifications_sent: std::collections::HashMap::new(),
        review_obligation_resends_sent: std::collections::HashMap::new(),
        review_obligation_failures_reported: std::collections::HashSet::new(),
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
        last_panesmith_snapshot_panes: BTreeSet::new(),
        force_panesmith_snapshot_refresh: true,
        project_config_loader: crate::run::no_project_config_loader(),
        last_budget_check: now - super::budget::DEFAULT_BUDGET_CHECK_INTERVAL,
        budget_check_interval: super::budget::DEFAULT_BUDGET_CHECK_INTERVAL,
        budget_torn_down: false,
        budget_block_dispatch: None,
        last_budget_warn: None,
        budget_event_sink: Some(super::budget::default_budget_event_sink()),
        needs_redraw: false,
        runtime_agent_factory_host_owned,
        runtime_terminal_host_absolute_resize: false,
    })
}

pub(super) fn run(ctx: &mut EventLoopCtx) -> io::Result<()> {
    while !ctx.shutdown.load(Ordering::Relaxed) {
        drain_runtime_command_receiver(ctx);
        drain_runtime_events_from_daemon(ctx);

        // Pre-drain: service any keystrokes that already accumulated so
        // they don't wait behind the (potentially long) PTY output
        // pipeline below. See § F8a in tmp/tick-latency/GOAL_PROMPT.md.
        // `focused_id` / `active_left_id` are recomputed because the
        // canonical versions (used by the render path) aren't in scope
        // yet at this point in the tick — they're built post-render
        // around line 1949 / 1965 below.
        let pre_focused_id = ctx.mux.focused_id().map(str::to_string);
        let pre_active_left_id = active_left_pane_id(ctx);
        let pre_drain =
            drain_pending_input(ctx, Duration::ZERO, &pre_focused_id, &pre_active_left_id)?;
        if pre_drain.should_break {
            break;
        }
        let visible_active_left_id = active_left_pane_id(ctx);
        let panesmith_snapshot_panes =
            visible_panesmith_snapshot_panes(ctx, visible_active_left_id.as_deref());

        let (_total_bytes, batch_events) = ctx
            .mux
            .poll_batch_with_panesmith_snapshot_panes(&panesmith_snapshot_panes);
        ctx.mux.flush_pending_inbox_nudges(&ctx.rt);
        let loop_now = std::time::Instant::now();

        process_pending_runtime_commands(ctx);
        process_pending_runtime_approval_resolutions(ctx);

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
                        if let Some(issue) = &ctx.last_shared_root_issue {
                            push_dashboard_event(
                                &ctx.dashboard_data,
                                format!(
                                    "Stopping Brehon immediately after shared-root mutation detection: {issue}"
                                ),
                            );
                            ctx.shutdown.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "Dashboard refresh task failed");
                    }
                }
            }
        }

        process_pending_queued_gateway_prompt_deliveries(ctx);

        let visible_event_seen = batch_events.iter().any(|event| {
            mux_event_affects_visible_ui(
                event,
                visible_active_left_id.as_deref(),
                ctx.supervisor_id.as_deref(),
                ctx.runtime_agent_factory_host_owned,
            )
        });

        // Mark dirty when visible pane output arrives. Hidden pane output is
        // still drained and reflected in runtime/session state, but it must
        // not force the operator dashboard into a full redraw loop.
        if visible_event_seen {
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

        let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
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
                ctx.dashboard_data.lock().tasks = refreshed_tasks;
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

        // Panic firewall around the untrusted-event-driven supervisor reset
        // seam: `batch_events` carries agent-produced pane output, so a panic
        // in reset detection must not tear down the run loop. The fn returns
        // () and recomputes its candidates from `batch_events`/`ctx` each
        // tick, so continuing after a caught panic is sound. The `?`/break/
        // continue IO paths in this loop body intentionally stay UNWRAPPED —
        // a terminal/IO failure is a legitimate stop condition.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            detect_and_handle_supervisor_resets(ctx, &batch_events, loop_now)
        }))
        .is_err()
        {
            tracing::error!("panic in supervisor reset detection; continuing event loop");
        }

        // Determine which left pane is active
        let active_left_id = active_left_pane_id(ctx);
        let focused_id = ctx.mux.focused_id().map(str::to_string);
        let dashboard_snapshot = ctx.dashboard_data.lock().clone();
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

        // Keep active timers moving only for panes currently on screen.
        if !ctx.needs_redraw
            && visible_structured_pane_has_active_tool(
                ctx,
                active_left_id.as_deref(),
                ctx.supervisor_id.as_deref(),
            )
        {
            ctx.needs_redraw = true;
        }

        if ctx.needs_redraw {
            if ctx.pending_initial_resize {
                if let Ok(size) = ctx.terminal.size() {
                    let terminal_size = size.into();
                    resize_panes(ctx, &terminal_size);
                }
                ctx.pending_initial_resize = false;
            }
            let panesmith_snapshot_refresh_targets = panesmith_snapshot_refresh_targets(
                &panesmith_snapshot_panes,
                &ctx.last_panesmith_snapshot_panes,
                ctx.force_panesmith_snapshot_refresh,
            );
            for pane_id in &panesmith_snapshot_refresh_targets {
                let _ = ctx.mux.refresh_panesmith_snapshot(pane_id);
            }
            ctx.last_panesmith_snapshot_panes = panesmith_snapshot_panes.clone();
            ctx.force_panesmith_snapshot_refresh = false;
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
                        Some(&ctx.mux),
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

            let brehon_root = ctx.dashboard_data.lock().brehon_root.clone();
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

        // Panic firewalls around the periodic ticks: a panic in stall
        // detection or the budget kill-switch must NOT unwind out of the
        // run loop and end a multi-day unattended session. AssertUnwindSafe
        // is sound here because both fns return () and mutate only plain
        // ctx fields with no cross-field invariant a half-finished tick
        // would corrupt: stall detection recomputes from `ctx` every tick,
        // and the budget teardown is latched idempotent via
        // `ctx.budget_torn_down`, so re-running next tick after a caught
        // panic cannot double-act. See docs/adr/0009-panic-unwind-firewalls.md.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::stall_handling::detect_and_handle_stalls(ctx)
        }))
        .is_err()
        {
            tracing::error!("panic in detect_and_handle_stalls; continuing event loop");
        }
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::budget::budget_tick(ctx)
        }))
        .is_err()
        {
            tracing::error!("panic in budget_tick; continuing event loop");
        }
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
    use crate::run::budget;
    use crate::run::init_test_git_repo;
    use crate::run::prompt_delivery::dispatch_runtime_prompt;
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TestRouteRejection {
        SendPrompt(String),
        RecyclePane(String),
        ResetPane(String),
    }

    struct SelectiveRecordingRouter {
        tx: std::sync::Mutex<mpsc::Sender<RecordedRoute>>,
        rejections: Vec<TestRouteRejection>,
    }

    #[test]
    fn hidden_pane_output_does_not_invalidate_visible_ui() {
        let event = MuxEvent::PaneOutput {
            pane_id: "hidden-worker".to_string(),
            data: Vec::new(),
            generation: brehon_mux::Generation(1),
        };

        assert!(!mux_event_affects_visible_ui(
            &event,
            Some("visible-worker"),
            Some("claude-supervisor"),
            false,
        ));
    }

    #[test]
    fn visible_pane_output_invalidates_visible_ui() {
        let event = MuxEvent::PaneOutput {
            pane_id: "visible-worker".to_string(),
            data: Vec::new(),
            generation: brehon_mux::Generation(1),
        };

        assert!(mux_event_affects_visible_ui(
            &event,
            Some("visible-worker"),
            Some("claude-supervisor"),
            false,
        ));
    }

    #[test]
    fn supervisor_output_invalidates_visible_ui_when_embedded() {
        let event = MuxEvent::PaneOutput {
            pane_id: "claude-supervisor".to_string(),
            data: Vec::new(),
            generation: brehon_mux::Generation(1),
        };

        assert!(mux_event_affects_visible_ui(
            &event,
            None,
            Some("claude-supervisor"),
            false,
        ));
        assert!(!mux_event_affects_visible_ui(
            &event,
            None,
            Some("claude-supervisor"),
            true,
        ));
    }

    #[test]
    fn panesmith_snapshot_refresh_targets_skip_stable_visible_set() {
        let current = ["worker-a", "claude-supervisor"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>();
        let previous = current.clone();

        let targets = panesmith_snapshot_refresh_targets(&current, &previous, false);

        assert!(
            targets.is_empty(),
            "stable visible panes must not force expensive snapshot refreshes"
        );
    }

    #[test]
    fn panesmith_snapshot_refresh_targets_include_new_or_forced_panes() {
        let current = ["worker-a", "claude-supervisor"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>();
        let previous = ["claude-supervisor"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>();

        let targets = panesmith_snapshot_refresh_targets(&current, &previous, false);
        assert_eq!(targets, BTreeSet::from(["worker-a".to_string()]));

        let forced = panesmith_snapshot_refresh_targets(&current, &previous, true);
        assert_eq!(forced, current);
    }

    #[test]
    fn dashboard_default_tick_rate_is_cpu_bounded() {
        let mux = Mux::new(24, 80);
        let harness = harness_with_mux(mux);

        assert_eq!(harness.ctx.tick_active, Duration::from_millis(50));
        assert_eq!(harness.ctx.tick_idle, Duration::from_millis(200));
    }

    impl SelectiveRecordingRouter {
        fn should_reject(&self, command: &RuntimeCommand) -> bool {
            self.rejections.iter().any(|rejection| match rejection {
                TestRouteRejection::SendPrompt(target) => {
                    matches!(
                        &command.kind,
                        RuntimeCommandKind::SendPrompt { .. }
                            if command.target.pane_id.as_deref() == Some(target.as_str())
                    )
                }
                TestRouteRejection::RecyclePane(target) => {
                    matches!(
                        &command.kind,
                        RuntimeCommandKind::RecyclePane { .. }
                            if command.target.pane_id.as_deref() == Some(target.as_str())
                    )
                }
                TestRouteRejection::ResetPane(target) => {
                    matches!(
                        &command.kind,
                        RuntimeCommandKind::ResetPane { .. }
                            if command.target.pane_id.as_deref() == Some(target.as_str())
                    )
                }
            })
        }
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

    #[async_trait::async_trait]
    impl RuntimeCommandRouter for SelectiveRecordingRouter {
        async fn route_command(
            &self,
            command: RuntimeCommand,
            context: RuntimePolicyContext,
        ) -> Result<brehon_types::RuntimeCommandResult, PortError> {
            let command_id = command.command_id.clone();
            let rejected = self.should_reject(&command);
            self.tx
                .lock()
                .expect("router lock")
                .send(RecordedRoute { command, context })
                .expect("record route");
            Ok(brehon_types::RuntimeCommandResult {
                command_id,
                status: if rejected {
                    RuntimeCommandStatus::Rejected
                } else {
                    RuntimeCommandStatus::Applied
                },
                message: Some(if rejected {
                    "rejected for test".to_string()
                } else {
                    "recorded".to_string()
                }),
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
            worktree_root: None,
            cargo_target_root: None,
            worktree_cleanup: brehon_types::WorktreeCleanupConfig::default(),
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
            None,
        )
        .expect("worker pane")
    }

    fn make_terminal_worker_pane(name: &str) -> brehon_mux::Pane {
        let dir = tempfile::tempdir().expect("tempdir");
        brehon_mux::Pane::worker(
            name,
            dir.path().to_path_buf(),
            None,
            "supervisor",
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Claude),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("terminal worker pane")
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
            None,
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
            None,
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
        let (tx, rx) = mpsc::channel();
        let router: Arc<dyn RuntimeCommandRouter> = Arc::new(RecordingRouter {
            tx: std::sync::Mutex::new(tx),
        });
        harness_with_runtime_router(mux, host_owned, router, rx)
    }

    fn harness_with_selective_router(
        mux: Mux,
        host_owned: bool,
        rejections: Vec<TestRouteRejection>,
    ) -> TestHarness {
        let (tx, rx) = mpsc::channel();
        let router: Arc<dyn RuntimeCommandRouter> = Arc::new(SelectiveRecordingRouter {
            tx: std::sync::Mutex::new(tx),
            rejections,
        });
        harness_with_runtime_router(mux, host_owned, router, rx)
    }

    fn harness_with_runtime_router(
        mux: Mux,
        host_owned: bool,
        router: Arc<dyn RuntimeCommandRouter>,
        rx: mpsc::Receiver<RecordedRoute>,
    ) -> TestHarness {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let rt_handle = rt.handle().clone();
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
        let dashboard_data = Arc::new(parking_lot::Mutex::new(DashboardData::default()));
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
                tick_active: Duration::from_millis(50),
                tick_idle: Duration::from_millis(200),
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
                review_obligation_nudge_threshold: Duration::from_secs(60),
                review_obligation_reset_threshold: Duration::from_secs(120),
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
                prompt_blocked_recovery_failed_panes: std::collections::HashSet::new(),
                post_checkpoint_nudge_threshold: Duration::from_secs(60),
                post_checkpoint_nudge_cooldown: Duration::from_secs(60),
                post_checkpoint_nudges_sent: std::collections::HashMap::new(),
                review_obligation_notifications_sent: std::collections::HashMap::new(),
                review_obligation_resends_sent: std::collections::HashMap::new(),
                review_obligation_failures_reported: std::collections::HashSet::new(),
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
                last_panesmith_snapshot_panes: BTreeSet::new(),
                force_panesmith_snapshot_refresh: true,
                project_config_loader: crate::run::no_project_config_loader(),
                last_budget_check: now - crate::run::budget::DEFAULT_BUDGET_CHECK_INTERVAL,
                budget_check_interval: crate::run::budget::DEFAULT_BUDGET_CHECK_INTERVAL,
                budget_torn_down: false,
                budget_block_dispatch: None,
                last_budget_warn: None,
                budget_event_sink: None,
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

    fn recv_available_routes(rx: &mpsc::Receiver<RecordedRoute>) -> Vec<RecordedRoute> {
        let mut routes = Vec::new();
        while let Ok(route) = rx.recv_timeout(Duration::from_millis(50)) {
            routes.push(route);
        }
        routes
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
                "reviewer_prompts": {
                    "reviewer-1": canonical_review_request_prompt_fixture()
                },
                "context": "Focused context"
            })
            .to_string(),
        )
        .expect("review request");
    }

    fn canonical_review_request_prompt_fixture() -> String {
        "Review request REV-review for task T-review: Pending review task\n\
Panel: primary\n\
Round: 1\n\
Description: Review the pending change\n\
Source: branch worker-branch (will merge into main)\n\
Commit: abc1234\n\
Base: def5678\n\
Review fingerprint:\n\
- review_round: 1\n\
- base_commit: def5678\n\
- merge_target_head: main\n\
\n\
Review handoff context:\n\
Focused context\n\
\n\
Research context:\n\
Research says look closely.\n\
\n\
Recorded proof of work so far:\n\
  Commands: 1\n\
\n\
Inspecting the commit:\n\
- All Brehon worktrees share one .git object database. The commit is reachable from your current worktree by SHA.\n\
- git show abc1234 --stat\n\
- git show abc1234\n\
- git diff def5678..abc1234\n\
- git log abc1234 -1\n\
\n\
Path interpretation:\n\
Paths: treat all file paths as repository-relative to your current worktree root.\n\
\n\
Review for: correctness, security, performance, concurrency, error handling, and maintainability.\n\
\n\
Submit your review (IMPORTANT: include reviewer=reviewer-1):\n\
  verification action=submit_review review_id=REV-review reviewer=reviewer-1 score=<1-10> verdict=<approved|needs_revision|rejected> summary=\"Your review\" findings='[{\"description\":\"...\", \"file\":\"...\", \"line\":42, \"severity\":\"blocking|suggestion|nitpick\", \"suggestion\":\"optional\"}]'\n\
\n\
Do not call request_review, reseat_panel, reassign_panel, release_panel, reset_rounds, or override."
            .to_string()
    }

    fn sanitize_prompt_key(prompt_id: &str) -> String {
        prompt_id
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

    fn write_prompt_enqueue_ack(brehon_root: &std::path::Path, prompt_id: &str, target: &str) {
        let ack_dir = brehon_root.join("runtime").join("prompt-enqueue-acks");
        std::fs::create_dir_all(&ack_dir).expect("enqueue ack dir");
        std::fs::write(
            ack_dir.join(format!("{}.json", sanitize_prompt_key(prompt_id))),
            serde_json::json!({
                "prompt_id": prompt_id,
                "target": target,
                "queued_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("prompt enqueue ack");
    }

    fn write_review_obligation_fixture_with_uncertain_delivery(brehon_root: &std::path::Path) {
        write_review_obligation_fixture(brehon_root);
        let runtime_dir = brehon_root.join("runtime");
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
                "reviewer_assignments": {
                    "reviewer-1": {
                        "owner": "reviewer-1",
                        "assignment_kind": "review",
                        "assigned_at": chrono::Utc::now().to_rfc3339(),
                        "prompt_id": "prompt-reviewer-1",
                        "delivery_method": "queued"
                    }
                },
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("review state with delivery");
        write_prompt_enqueue_ack(brehon_root, "prompt-reviewer-1", "reviewer-1");
    }

    fn write_multi_reviewer_phase_gate_fixture(brehon_root: &std::path::Path) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        let review_round_dir = runtime_dir
            .join("reviews")
            .join("T-phase-gate")
            .join("round-1");
        std::fs::create_dir_all(&review_round_dir).expect("review dir");
        std::fs::write(
            runtime_dir.join("tasks").join("T-phase-gate.json"),
            serde_json::json!({
                "task_id": "T-phase-gate",
                "title": "Phase gate task",
                "status": "in_review",
                "task_type": "task"
            })
            .to_string(),
        )
        .expect("task file");
        std::fs::write(
            runtime_dir
                .join("reviews")
                .join("T-phase-gate")
                .join("state.json"),
            serde_json::json!({
                "task_id": "T-phase-gate",
                "status": "collecting",
                "current_round": 1,
                "current_review_id": "REV-phase-gate",
                "max_rounds": 3,
                "panel_id": "primary",
                "panel": ["reviewer-active", "reviewer-resend", "reviewer-reset", "reviewer-missing"],
                "submissions_received": ["reviewer-active"],
                "reviewer_assignments": {
                    "reviewer-resend": {
                        "owner": "reviewer-resend",
                        "assignment_kind": "review",
                        "assigned_at": chrono::Utc::now().to_rfc3339(),
                        "prompt_id": "prompt-reviewer-resend",
                        "delivery_method": "queued"
                    },
                    "reviewer-reset": {
                        "owner": "reviewer-reset",
                        "assignment_kind": "review",
                        "assigned_at": chrono::Utc::now().to_rfc3339(),
                        "acknowledged_at": chrono::Utc::now().to_rfc3339(),
                        "prompt_id": "prompt-reviewer-reset",
                        "delivery_method": "queued"
                    },
                    "reviewer-missing": {
                        "owner": "reviewer-missing",
                        "assignment_kind": "review",
                        "assigned_at": chrono::Utc::now().to_rfc3339(),
                        "prompt_id": "prompt-reviewer-missing",
                        "delivery_method": "queued"
                    }
                },
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("review state");
        std::fs::write(
            review_round_dir.join("request.json"),
            serde_json::json!({
                "task_id": "T-phase-gate",
                "review_id": "REV-phase-gate",
                "requested_by": "supervisor",
                "requested_at": chrono::Utc::now().to_rfc3339(),
                "title": "Phase gate task",
                "description": "Review the phase gate diff",
                "commit": "abc1234",
                "base_commit": "def5678",
                "merge_target_head": "epic/phase-1",
                "commits": ["abc1234"],
                "reviewer_prompts": {
                    "reviewer-resend": "Canonical resend prompt for reviewer-resend",
                    "reviewer-reset": "Canonical resend prompt for reviewer-reset",
                    "reviewer-missing": "Canonical resend prompt for reviewer-missing"
                },
                "context": "Phase 1 gate context"
            })
            .to_string(),
        )
        .expect("review request");
        write_prompt_enqueue_ack(brehon_root, "prompt-reviewer-resend", "reviewer-resend");
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

    fn write_active_assigned_task_with_assignment_fixture(
        brehon_root: &std::path::Path,
        assigned_at: chrono::DateTime<chrono::Utc>,
        progress_started: bool,
    ) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        let mut assignment_propagation = serde_json::json!({
            "owner": "worker-1",
            "assignment_kind": "task",
            "assigned_at": assigned_at.to_rfc3339(),
            "prompt_id": "prompt-worker-1",
            "delivery_method": "queued",
            "acknowledged_at": (assigned_at + chrono::Duration::seconds(5)).to_rfc3339(),
            "acknowledged_by": "worker-1",
            "acknowledged_via": "task action=mine"
        });
        if progress_started {
            assignment_propagation["progress_started_at"] = serde_json::Value::String(
                (assigned_at + chrono::Duration::seconds(10)).to_rfc3339(),
            );
            assignment_propagation["progress_started_by"] =
                serde_json::Value::String("worker-1".to_string());
            assignment_propagation["progress_started_via"] =
                serde_json::Value::String("task action=progress".to_string());
        }
        std::fs::write(
            runtime_dir.join("tasks").join("T-owned.json"),
            serde_json::json!({
                "task_id": "T-owned",
                "title": "Owned task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1",
                "assignment_propagation": assignment_propagation
            })
            .to_string(),
        )
        .expect("task file");
    }

    fn write_inactive_task_fixture(brehon_root: &std::path::Path, task_id: &str) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        std::fs::write(
            runtime_dir.join("tasks").join(format!("{task_id}.json")),
            serde_json::json!({
                "task_id": task_id,
                "title": "Inactive task",
                "status": "pending",
                "task_type": "task"
            })
            .to_string(),
        )
        .expect("task file");
    }

    fn write_terminal_task_fixture(brehon_root: &std::path::Path, task_id: &str, status: &str) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        std::fs::write(
            runtime_dir.join("tasks").join(format!("{task_id}.json")),
            serde_json::json!({
                "task_id": task_id,
                "title": "Terminal task",
                "status": status,
                "task_type": "task"
            })
            .to_string(),
        )
        .expect("task file");
    }

    fn write_agent_session_fixture(brehon_root: &std::path::Path, agent_id: &str, role: &str) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("sessions")).expect("sessions dir");
        std::fs::write(
            runtime_dir
                .join("sessions")
                .join(format!("{agent_id}.json")),
            serde_json::json!({
                "name": agent_id,
                "role": role,
                "session_id": format!("session-{agent_id}"),
                "last_seen_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("session file");
    }

    fn write_worker_session_fixture(brehon_root: &std::path::Path, worker_id: &str) {
        write_agent_session_fixture(brehon_root, worker_id, "worker");
    }

    fn write_worker_worktree_fixture(
        brehon_root: &std::path::Path,
        worker_id: &str,
    ) -> std::path::PathBuf {
        let worktree = brehon_root
            .join("worktrees")
            .join("runs")
            .join("run-1")
            .join(worker_id);
        init_test_git_repo(&worktree);
        worktree
    }

    fn write_invalid_agent_health_path_fixture(brehon_root: &std::path::Path) {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(&runtime_dir).expect("runtime dir");
        std::fs::write(runtime_dir.join("agent-health"), "not a directory")
            .expect("invalid agent-health path");
    }

    fn write_prompt_blocked_worker_health_fixture(
        brehon_root: &std::path::Path,
        worker_id: &str,
        task_id: &str,
    ) -> std::path::PathBuf {
        write_prompt_blocked_health_fixture(brehon_root, worker_id, Some(task_id))
    }

    fn write_taskless_prompt_blocked_health_fixture(
        brehon_root: &std::path::Path,
        worker_id: &str,
    ) -> std::path::PathBuf {
        write_prompt_blocked_health_fixture(brehon_root, worker_id, None)
    }

    fn write_prompt_blocked_health_fixture(
        brehon_root: &std::path::Path,
        worker_id: &str,
        task_id: Option<&str>,
    ) -> std::path::PathBuf {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("agent-health")).expect("health dir");
        let health_path = runtime_dir
            .join("agent-health")
            .join(format!("{worker_id}.json"));
        let blocked = match task_id {
            Some(task_id) => serde_json::json!({
                "kind": "permission_request",
                "summary": "permission request blocked automatic recovery: allow bash ls",
                "command_or_tool": "allow bash ls",
                "request_id": "perm-1",
                "task_id": task_id
            }),
            None => serde_json::json!({
                "kind": "permission_request",
                "summary": "permission request blocked automatic recovery: allow bash ls",
                "command_or_tool": "allow bash ls",
                "request_id": "perm-1"
            }),
        };
        std::fs::write(
            &health_path,
            serde_json::json!({
                "agent": worker_id,
                "status": "unavailable",
                "reason": "prompt_blocked",
                "blocked": blocked
            })
            .to_string(),
        )
        .expect("health file");
        health_path
    }

    fn write_quarantined_worker_health_fixture(
        brehon_root: &std::path::Path,
        worker_id: &str,
    ) -> std::path::PathBuf {
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("agent-health")).expect("health dir");
        let health_path = runtime_dir
            .join("agent-health")
            .join(format!("{worker_id}.json"));
        std::fs::write(
            &health_path,
            serde_json::json!({
                "agent": worker_id,
                "status": "unavailable",
                "reason": "quota_exhausted"
            })
            .to_string(),
        )
        .expect("health file");
        health_path
    }

    fn apply_prompt_blocked_runtime_state(mux: &mut Mux, pane_id: &str, task_id: &str) {
        apply_prompt_blocked_runtime_state_with_task(mux, pane_id, Some(task_id));
    }

    fn apply_taskless_prompt_blocked_runtime_state(mux: &mut Mux, pane_id: &str) {
        apply_prompt_blocked_runtime_state_with_task(mux, pane_id, None);
    }

    fn apply_prompt_blocked_runtime_state_with_task(
        mux: &mut Mux,
        pane_id: &str,
        task_id: Option<&str>,
    ) {
        let generation = mux
            .get(pane_id)
            .expect("prompt-blocked pane")
            .current_generation();
        let event = brehon_types::RuntimeEvent::new(
            brehon_types::RuntimeEventMeta::new(
                "test-session",
                pane_id,
                generation.0,
                brehon_types::RuntimeSource::Headless,
                1,
            ),
            brehon_types::RuntimeEventKind::PaneStateChanged(brehon_types::PaneStateChangedEvent {
                previous: Some(brehon_types::RuntimePaneState::Ready),
                current: brehon_types::RuntimePaneState::Blocked,
                reason: Some("prompt blocked".to_string()),
                blocked: Some(brehon_types::RuntimePaneBlockInfo {
                    kind: brehon_types::RuntimePaneBlockKind::PermissionRequest,
                    summary: "permission request blocked automatic recovery: allow bash ls"
                        .to_string(),
                    command_or_tool: Some("allow bash ls".to_string()),
                    request_id: Some("perm-1".to_string()),
                    task_id: task_id.map(str::to_string),
                    excerpt: None,
                }),
            }),
        );
        assert!(
            mux.apply_terminal_host_runtime_event(&event)
                .expect("blocked runtime event"),
            "blocked runtime event should update the pane state",
        );
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
    fn ctrl_f_attaches_focused_panesmith_pane() {
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
        let mut harness = harness_with_mux(mux);
        let ctrl_f = crossterm::event::KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        let raw_ctrl_f =
            crossterm::event::KeyEvent::new(KeyCode::Char('\u{6}'), KeyModifiers::empty());
        let raw_ctrl_f_with_modifier =
            crossterm::event::KeyEvent::new(KeyCode::Char('\u{6}'), KeyModifiers::CONTROL);

        assert!(should_attach_focused_panesmith_pane(&harness.ctx, &ctrl_f));
        assert!(should_attach_focused_panesmith_pane(
            &harness.ctx,
            &raw_ctrl_f
        ));
        assert!(should_attach_focused_panesmith_pane(
            &harness.ctx,
            &raw_ctrl_f_with_modifier
        ));
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(harness.ctx.mux.shutdown_all());

        let mut shell_mux = Mux::new(24, 80);
        let shell_id = shell_mux
            .add_shell("shell", std::env::temp_dir(), Some("cat"))
            .expect("add shell");
        shell_mux.focus(&shell_id);
        let mut shell_harness = harness_with_mux(shell_mux);
        assert!(should_attach_focused_panesmith_pane(
            &shell_harness.ctx,
            &ctrl_f
        ));
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(shell_harness.ctx.mux.shutdown_all());

        let mut ghostty_mux = Mux::new(24, 80);
        ghostty_mux.add_pane(make_supervisor_pane("claude-supervisor"));
        ghostty_mux.focus("claude-supervisor");
        let ghostty_harness = harness_with_mux(ghostty_mux);

        assert!(!should_attach_focused_panesmith_pane(
            &ghostty_harness.ctx,
            &ctrl_f
        ));
        assert!(!should_attach_focused_panesmith_pane(
            &ghostty_harness.ctx,
            &raw_ctrl_f
        ));
        assert!(!should_attach_focused_panesmith_pane(
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
    fn manual_reset_context_never_requires_approval() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("supervisor"));
        let mut harness = harness_with_mux(mux);

        assert!(perform_manual_reset_request(&mut harness.ctx, "supervisor"));

        let routed = recv_route(&harness.rx);
        assert!(
            !routed.context.approval_required,
            "manual reset must not wait on a human approval path"
        );

        drain_runtime_commands(&mut harness);
    }

    #[test]
    fn supervisor_auto_reset_context_never_requires_approval() {
        let mut mux = Mux::new(24, 80);
        let mut pane = make_supervisor_pane("claude-supervisor");
        pane.append_output(
            br#"<anonymous> (/bunfs/root/src/entrypoints/cli.js:577:98876)
TypeError: Cannot read properties of undefined"#,
        )
        .expect("append crash output");
        mux.add_pane(pane);

        // Verify the crash scenario is detected before routing
        assert_eq!(
            supervisor_reset_reason(&mux, "claude-supervisor"),
            Some("runtime crash"),
            "pane must be in crash state for this test to guard the right scenario"
        );

        let mut harness = harness_with_mux(mux);

        // data field is empty because supervisor_reset_reason reads from the
        // pane's output buffer (populated above via append_output), not from
        // the event payload.
        let batch_events = vec![MuxEvent::PaneOutput {
            pane_id: "claude-supervisor".to_string(),
            data: vec![],
            generation: brehon_mux::Generation(0),
        }];
        // Instant::now() works here because last_supervisor_reset is empty on a
        // fresh harness, so the cooldown check always passes.
        detect_and_handle_supervisor_resets(&mut harness.ctx, &batch_events, Instant::now());

        let routed = recv_route(&harness.rx);
        assert_eq!(
            routed.command.target.pane_id.as_deref(),
            Some("claude-supervisor")
        );
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason } if reason == "supervisor runtime failure: runtime crash"
        ));
        assert!(
            !routed.context.approval_required,
            "supervisor auto-reset must not wait on a human approval path"
        );

        drain_runtime_commands(&mut harness);
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
    fn stall_recycle_context_never_requires_approval() {
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
        assert!(
            !routed.context.approval_required,
            "stall recycle must not wait on a human approval path"
        );

        drain_runtime_commands(&mut harness);
    }

    // ── Budget kill-switch integration ───────────────────────────────────
    //
    // Drives the REAL gate (`budget_tick` + the `dispatch_runtime_prompt`
    // seam) against an injected fake spend source (a tempdir rollup over a
    // Hard token cap, surfaced through the real config loader) and a fake
    // event sink. Asserts: over-cap => new dispatch refused, teardown invoked
    // once, and a breach event emitted. This is caused by the production gate,
    // not hand-appended events.

    fn write_budget_project_config(project_root: &std::path::Path, body: &str) {
        let brehon_dir = project_root.join(".brehon");
        std::fs::create_dir_all(&brehon_dir).unwrap();
        std::fs::write(brehon_dir.join("config.yaml"), body).unwrap();
    }

    fn write_initiative_rollup(brehon_root: &std::path::Path, task_id: &str, tokens: u64) {
        let tasks = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(
            tasks.join(format!("{task_id}.json")),
            serde_json::json!({
                "task_id": task_id,
                "task_type": "initiative",
                "token_usage": { "tokens_used": tokens }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn project_config_loader_from_disk() -> crate::run::research::ProjectConfigLoader {
        Arc::new(|project_root: &std::path::Path| {
            brehon_config::load_config(Some(project_root)).ok()
        })
    }

    #[test]
    fn budget_tick_over_hard_cap_refuses_dispatch_and_tears_down_once() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);

        // Project layout: <tempdir>/.brehon is the brehon_root; the config
        // (Hard, 1000-token cap) lives under it; the rollup is 5000 tokens.
        let project = tempfile::tempdir().unwrap();
        let brehon_root = project.path().join(".brehon");
        write_budget_project_config(
            project.path(),
            "budget:\n  max_tokens_per_agent: 1000\n  enforcement: Hard\n",
        );
        write_initiative_rollup(&brehon_root, "I-1", 5_000);

        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());
        harness.ctx.project_config_loader = project_config_loader_from_disk();

        // Fake durable event sink.
        let breaches: Arc<std::sync::Mutex<Vec<budget::BudgetBreachEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_breaches = breaches.clone();
        harness.ctx.budget_event_sink = Some(Arc::new(move |event| {
            sink_breaches.lock().unwrap().push(event);
        }));

        // Force the tick to run immediately.
        harness.ctx.budget_check_interval = Duration::ZERO;
        harness.ctx.last_budget_check = Instant::now() - Duration::from_secs(60);

        // Before the breach, dispatch is allowed.
        assert!(budget::dispatch_allowed(&harness.ctx).is_ok());

        budget::budget_tick(&mut harness.ctx);

        // (a) NEW dispatch is refused after the breach.
        assert!(
            !dispatch_runtime_prompt(&mut harness.ctx, "worker-1", "go".to_string(), None),
            "dispatch must be refused once the Hard cap is breached"
        );

        // (b) Teardown invoked exactly once.
        assert!(harness.ctx.budget_torn_down, "teardown latch must be set");
        assert!(
            harness.ctx.shutdown.load(Ordering::SeqCst),
            "shutdown must be requested so the post-loop teardown fires"
        );

        // (c) A breach event was recorded through the injected sink.
        {
            let recorded = breaches.lock().unwrap();
            assert_eq!(recorded.len(), 1, "exactly one breach event");
            assert!(
                recorded[0].reason.contains("token limit"),
                "breach names the cap: {}",
                recorded[0].reason
            );
            assert_eq!(
                recorded[0].enforcement,
                brehon_types::BudgetEnforcement::Hard
            );
        }

        // (d) A budget-exceeded dashboard event is present.
        {
            let dash = harness.ctx.dashboard_data.lock();
            assert!(
                dash.events
                    .iter()
                    .any(|e| e.description.contains("budget exceeded")),
                "a budget-exceeded dashboard event must be present"
            );
        }

        // One-shot: a second tick must NOT re-emit or re-kill.
        harness.ctx.last_budget_check = Instant::now() - Duration::from_secs(60);
        budget::budget_tick(&mut harness.ctx);
        assert_eq!(
            breaches.lock().unwrap().len(),
            1,
            "the one-shot latch must prevent a second breach event"
        );
    }

    #[test]
    fn budget_tick_no_cap_never_tears_down_owner_unlimited_run() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);

        let project = tempfile::tempdir().unwrap();
        let brehon_root = project.path().join(".brehon");
        // Hard enforcement but NO numeric cap: an unlimited run.
        write_budget_project_config(project.path(), "budget:\n  enforcement: Hard\n");
        write_initiative_rollup(&brehon_root, "I-1", 50_000_000);

        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root);
        harness.ctx.project_config_loader = project_config_loader_from_disk();
        harness.ctx.budget_check_interval = Duration::ZERO;
        harness.ctx.last_budget_check = Instant::now() - Duration::from_secs(60);

        budget::budget_tick(&mut harness.ctx);

        assert!(
            !harness.ctx.budget_torn_down,
            "an unlimited run must never be torn down"
        );
        assert!(
            !harness.ctx.shutdown.load(Ordering::SeqCst),
            "an unlimited run must not request shutdown"
        );
        assert!(
            budget::dispatch_allowed(&harness.ctx).is_ok(),
            "dispatch must remain allowed for an unlimited run"
        );
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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

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
    fn prompt_blocked_active_worker_resets_immediately_and_clears_health() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-owned");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "successful prompt-blocked recovery should clear health marker"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("in_progress")
        );
        assert!(harness
            .ctx
            .last_worker_context_reset
            .contains_key("worker-1"));
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn quarantined_worker_reset_context_never_requires_approval() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("agent-health")).expect("health dir");

        write_active_assigned_task_fixture(&brehon_root);
        write_worker_session_fixture(&brehon_root, "worker-1");
        std::fs::write(
            runtime_dir.join("agent-health").join("worker-1.json"),
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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { .. }
        ));
        assert!(
            !routed.context.approval_required,
            "quarantined worker reset must not wait on a human approval path"
        );

        drain_runtime_commands(&mut harness);
    }

    #[test]
    fn prompt_blocked_active_worker_without_session_file_still_resets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-owned");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "prompt-blocked recovery should not require a session snapshot"
        );
        assert!(harness
            .ctx
            .last_worker_context_reset
            .contains_key("worker-1"));
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn prompt_blocked_worker_with_review_ready_task_context_resets_without_task_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-owned");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        mux.get_mut("worker-1")
            .expect("worker pane")
            .set_task_context(brehon_mux::TaskContextSnapshot {
                task_id: "T-owned".to_string(),
                title: "Review-ready task context".to_string(),
                // TaskContextSnapshot normalizes raw `review_ready` task files
                // to `TaskStatus::InReview`.
                status: brehon_types::task::TaskStatus::InReview,
                completion_mode: None,
                merge_target: None,
                parent_id: None,
                epic_branch: None,
                epic_worktree: None,
                blocked_reason: None,
                updated_at: Instant::now(),
            });
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "successful prompt-blocked recovery should clear the health marker"
        );
        assert!(harness
            .ctx
            .last_worker_context_reset
            .contains_key("worker-1"));
        assert!(
            harness.ctx.pending_runtime_commands.is_empty(),
            "reserved task context should trigger direct reset, not idle recycle"
        );
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn prompt_blocked_missing_worker_pane_blocks_marker_task_and_clears_health() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-owned");

        let mux = Mux::new(24, 80);
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "missing-pane prompt-blocked recovery should clear the stale health marker after blocking the task"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            task.get("activity").and_then(|value| value.as_str()),
            Some("prompt-blocked recovery failed")
        );
        assert!(harness.ctx.pending_runtime_commands.is_empty());
    }

    #[test]
    fn manual_reset_rejection_surfaces_explicit_runtime_failure() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("supervisor"));
        let mut harness = harness_with_selective_router(
            mux,
            false,
            vec![TestRouteRejection::ResetPane("supervisor".to_string())],
        );

        assert!(perform_manual_reset_request(&mut harness.ctx, "supervisor"));

        let routed = recv_route(&harness.rx);
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason } if reason == "manual reset"
        ));

        drain_runtime_commands(&mut harness);

        let dashboard = harness.ctx.dashboard_data.lock();
        let found = dashboard.events.iter().any(|event| {
            event
                .description
                .contains("manual reset for supervisor failed")
                && event.description.contains("rejected for test")
        });
        assert!(
            found,
            "manual reset rejection should produce an explicit Brehon runtime failure on the dashboard, not a hidden terminal prompt; got events: {:?}",
            dashboard.events
        );
    }

    #[test]
    fn stall_recycle_rejection_surfaces_explicit_runtime_failure_and_blocks_task() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("tasks")).expect("tasks dir");
        std::fs::write(
            runtime_dir.join("tasks").join("T-owned.json"),
            serde_json::json!({
                "task_id": "T-owned",
                "title": "Owned task",
                "status": "pending",
                "task_type": "task"
            })
            .to_string(),
        )
        .expect("task file");
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-owned");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_selective_router(
            mux,
            false,
            vec![TestRouteRejection::RecyclePane("worker-1".to_string())],
        );
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason.starts_with("auto-recover prompt-blocked idle worker pane:")
        ));

        drain_runtime_commands(&mut harness);

        let dashboard = harness.ctx.dashboard_data.lock();
        let found = dashboard.events.iter().any(|event| {
            event
                .description
                .contains("failed to recycle prompt-blocked idle worker worker-1")
                && event.description.contains("rejected for test")
        });
        assert!(
            found,
            "stall recycle rejection should produce an explicit Brehon runtime failure on the dashboard, not a hidden terminal prompt; got events: {:?}",
            dashboard.events
        );

        assert!(
            !health_path.exists(),
            "failed recycle should still clear the stale health marker"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("blocked"),
            "failed recycle should block the owning task with a clear operator-visible reason"
        );
        assert_eq!(
            task.get("activity").and_then(|value| value.as_str()),
            Some("prompt-blocked recovery failed")
        );
    }

    #[test]
    fn prompt_blocked_active_reviewer_resets_and_clears_health() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "reviewer-1", "T-review");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        mux.get_mut("reviewer-1")
            .expect("reviewer pane")
            .set_review_context(brehon_mux::ReviewContextSnapshot {
                review_id: "REV-review".to_string(),
                task_id: "T-review".to_string(),
                round: 1,
                panel_total: 1,
                panel_done: 0,
                verdict: None,
                score: None,
                findings_summary: None,
                updated_at: Instant::now(),
            });
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "successful prompt-blocked reviewer recovery should clear health marker"
        );
        let pane = harness.ctx.mux.get("reviewer-1").expect("reviewer pane");
        assert!(
            pane.review_context().is_none(),
            "reviewer reset should clear the active review context"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-review.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("in_review")
        );
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn prompt_blocked_missing_reviewer_pane_blocks_marker_task_and_clears_health() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);
        write_agent_session_fixture(&brehon_root, "reviewer-1", "reviewer");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "reviewer-1", "T-review");

        let mux = Mux::new(24, 80);
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "missing-pane reviewer recovery should clear the stale health marker after blocking the task"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-review.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            task.get("activity").and_then(|value| value.as_str()),
            Some("prompt-blocked recovery failed")
        );
        assert!(harness.ctx.pending_runtime_commands.is_empty());
    }

    #[test]
    fn prompt_blocked_supervisor_resets_and_clears_health() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let health_path = write_prompt_blocked_worker_health_fixture(
            &brehon_root,
            "claude-supervisor",
            "T-supervisor",
        );

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "successful prompt-blocked supervisor recovery should clear health marker"
        );
        assert!(harness
            .ctx
            .last_supervisor_reset
            .contains_key("claude-supervisor"));
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn prompt_blocked_idle_worker_recycles_with_startup_prompt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-idle");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        mux.get_mut("worker-1")
            .expect("worker pane")
            .set_task_context(brehon_mux::TaskContextSnapshot {
                task_id: "T-stale".to_string(),
                title: "Stale task context".to_string(),
                status: brehon_types::task::TaskStatus::Pending,
                completion_mode: None,
                merge_target: None,
                parent_id: None,
                epic_branch: None,
                epic_worktree: None,
                blocked_reason: None,
                updated_at: Instant::now(),
            });
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason.contains("auto-recover prompt-blocked idle worker pane")
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
        std::fs::remove_file(&health_path).expect("remove prompt-blocked marker before rejection");

        drain_runtime_commands(&mut harness);

        assert!(
            !health_path.exists(),
            "successful idle prompt-blocked recycle should clear health marker"
        );
        assert!(
            harness
                .ctx
                .mux
                .get("worker-1")
                .expect("worker pane")
                .task_context()
                .is_none(),
            "idle recycle should clear stale task context"
        );
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn prompt_blocked_idle_worker_does_not_queue_duplicate_recycles_while_pending() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_worker_session_fixture(&brehon_root, "worker-1");
        write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-idle");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);
        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason.contains("auto-recover prompt-blocked idle worker pane")
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert_eq!(
            harness.ctx.pending_runtime_commands.len(),
            1,
            "prompt-blocked idle worker should not queue duplicate recycle commands while one is already pending"
        );
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "no second recycle command should be routed while the first is still pending"
        );
    }

    #[test]
    fn prompt_blocked_idle_worker_blocks_task_when_recycle_router_is_unavailable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_inactive_task_fixture(&brehon_root, "T-idle");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-idle");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "blocking the task after failed idle recycle should clear health marker"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-idle.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            task.get("activity").and_then(|value| value.as_str()),
            Some("prompt-blocked recovery failed")
        );
        assert!(task
            .get("blockers")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.contains("runtime command router unavailable")));
    }

    #[test]
    fn prompt_blocked_idle_worker_blocks_task_when_queued_recycle_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_inactive_task_fixture(&brehon_root, "T-idle");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-idle");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_selective_router(
            mux,
            false,
            vec![TestRouteRejection::RecyclePane("worker-1".to_string())],
        );
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason.contains("auto-recover prompt-blocked idle worker pane")
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);

        drain_runtime_commands(&mut harness);

        assert!(
            !health_path.exists(),
            "blocking the task after rejected queued recycle should clear health marker"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-idle.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            task.get("activity").and_then(|value| value.as_str()),
            Some("prompt-blocked recovery failed")
        );
        assert!(task
            .get("blockers")
            .and_then(|value| value.as_str())
            .is_some_and(|value| {
                value.contains("rejected for test")
                    && value.contains("allow bash ls")
                    && value.contains("request_id perm-1")
            }));
        assert!(task
            .get("recovery_note")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.contains("allow bash ls")));
    }

    #[test]
    fn prompt_blocked_idle_worker_queued_recycle_terminal_task_failure_converges() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_terminal_task_fixture(&brehon_root, "T-idle", "merged");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-idle");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_selective_router(
            mux,
            false,
            vec![TestRouteRejection::RecyclePane("worker-1".to_string())],
        );
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason.contains("auto-recover prompt-blocked idle worker pane")
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);

        drain_runtime_commands(&mut harness);

        let first_marker =
            std::fs::read_to_string(&health_path).expect("terminal queued recycle failure marker");
        let first_marker_json =
            serde_json::from_str::<serde_json::Value>(&first_marker).expect("marker json");
        assert_eq!(
            first_marker_json
                .get("reason")
                .and_then(|value| value.as_str()),
            Some("prompt_blocked_recovery_failed")
        );
        assert!(first_marker_json
            .get("error")
            .and_then(|value| value.as_str())
            .is_some_and(|value| {
                value.contains("could not mark task T-idle blocked")
                    && value.contains("terminal task T-idle")
            }));
        assert_eq!(
            first_marker_json
                .get("blocked")
                .and_then(|value| value.get("command_or_tool"))
                .and_then(|value| value.as_str()),
            Some("allow bash ls")
        );
        let merged_task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-idle.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            merged_task.get("status").and_then(|value| value.as_str()),
            Some("merged")
        );
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_marker = std::fs::read_to_string(&health_path).expect("marker after resweep");
        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_marker, second_marker,
            "task-backed queued recycle failures that cannot rewrite a terminal task should converge to a one-shot terminal marker"
        );
        assert_eq!(
            first_event_count, second_event_count,
            "task-backed queued recycle failures that cannot rewrite a terminal task should not emit duplicate dashboard events after fallback marker convergence"
        );
    }

    #[test]
    fn prompt_blocked_idle_worker_queued_recycle_marker_write_failure_is_suppressed_in_memory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_terminal_task_fixture(&brehon_root, "T-idle", "merged");
        write_invalid_agent_health_path_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        apply_prompt_blocked_runtime_state_with_task(&mut mux, "worker-1", Some("T-idle"));
        let mut harness = harness_with_selective_router(
            mux,
            false,
            vec![TestRouteRejection::RecyclePane("worker-1".to_string())],
        );
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        drain_runtime_commands(&mut harness);

        assert!(
            harness
                .ctx
                .prompt_blocked_recovery_failed_panes
                .contains("worker-1"),
            "queued recycle failures should suppress the live blocked worker in memory when the recovery-failed marker cannot be persisted"
        );
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_event_count, second_event_count,
            "once in-memory suppression is recorded for a queued recycle failure, later stall sweeps should not retry the same blocked worker"
        );
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "suppressed queued recycle failures should not route a second recycle command"
        );
    }

    #[test]
    fn taskless_prompt_blocked_idle_worker_queued_recycle_failure_is_marked_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path = write_taskless_prompt_blocked_health_fixture(&brehon_root, "worker-1");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_selective_router(
            mux,
            false,
            vec![TestRouteRejection::RecyclePane("worker-1".to_string())],
        );
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason.contains("auto-recover prompt-blocked idle worker pane")
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
        std::fs::remove_file(&health_path)
            .expect("remove prompt-blocked marker before taskless rejection");

        drain_runtime_commands(&mut harness);

        let first_marker =
            std::fs::read_to_string(&health_path).expect("taskless queued failure marker");
        let marker = serde_json::from_str::<serde_json::Value>(&first_marker).expect("marker");
        assert_eq!(
            marker.get("reason").and_then(|value| value.as_str()),
            Some("prompt_blocked_recovery_failed")
        );
        assert_eq!(
            marker
                .get("blocked")
                .and_then(|value| value.get("command_or_tool"))
                .and_then(|value| value.as_str()),
            Some("allow bash ls")
        );
        assert_eq!(
            marker
                .get("blocked")
                .and_then(|value| value.get("request_id"))
                .and_then(|value| value.as_str()),
            Some("perm-1")
        );
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_marker = std::fs::read_to_string(&health_path).expect("marker after resweep");
        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_marker, second_marker,
            "queued recycle rejection for a taskless prompt-blocked worker should converge to a one-shot terminal marker"
        );
        assert_eq!(
            first_event_count, second_event_count,
            "queued recycle rejection for a taskless prompt-blocked worker should not emit duplicate dashboard events after the terminal marker is recorded"
        );
    }

    #[test]
    fn stale_dirty_active_assigned_worker_is_not_reset_after_nudge_window() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        let worktree = write_worker_worktree_fixture(&brehon_root, "worker-1");
        std::fs::write(worktree.join("dirty.txt"), "pending changes\n").expect("dirty worktree");

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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("Dirty worktree handoff required")
                    && text.contains("T-owned")
                    && text.contains("action=complete")
                    && text.contains("action=checkpoint")
        ));
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "in_progress");
        assert_eq!(task["assignee"], "worker-1");

        assert!(!harness
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
            "dirty assigned worker nudge must not loop within the cooldown"
        );
    }

    #[test]
    fn idle_dirty_assigned_worker_gets_handoff_nudge_before_reset_threshold() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        let worktree = write_worker_worktree_fixture(&brehon_root, "worker-1");
        std::fs::write(worktree.join("dirty.txt"), "pending changes\n").expect("dirty worktree");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60);
        harness.ctx.post_checkpoint_nudge_threshold = Duration::from_secs(1);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("Dirty worktree handoff required")
                    && text.contains("Shell output, prose, and local edits do not update Brehon state")
                    && text.contains("action=complete")
        ));
        assert!(harness
            .ctx
            .active_worker_recovery_nudges_sent
            .contains_key(&("worker-1".to_string(), "T-owned".to_string())));
    }

    #[test]
    fn stale_dirty_assigned_worker_gets_handoff_nudge_despite_recent_runtime_prompt_activity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_with_assignment_fixture(
            &brehon_root,
            chrono::Utc::now() - chrono::Duration::seconds(120),
            true,
        );
        let worktree = write_worker_worktree_fixture(&brehon_root, "worker-1");
        std::fs::write(worktree.join("dirty.txt"), "pending changes\n").expect("dirty worktree");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60);
        harness.ctx.post_checkpoint_nudge_threshold = Duration::from_secs(300);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("Dirty worktree handoff required")
                    && text.contains("without a lifecycle handoff")
                    && text.contains("action=checkpoint")
                    && text.contains("action=complete")
        ));
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "dirty lifecycle-stale worker should get exactly one handoff nudge"
        );
        assert!(harness
            .ctx
            .active_worker_recovery_nudges_sent
            .contains_key(&("worker-1".to_string(), "T-owned".to_string())));
    }

    #[test]
    fn active_assigned_worker_without_task_progress_gets_nudged_despite_recent_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_with_assignment_fixture(
            &brehon_root,
            chrono::Utc::now() - chrono::Duration::seconds(120),
            false,
        );
        write_worker_worktree_fixture(&brehon_root, "worker-1");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("No task progress recorded")
                    && text.contains("T-owned")
                    && text.contains("action=progress")
                    && text.contains("action=complete")
        ));
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "recent terminal output must only get a no-progress nudge on the first sweep"
        );
        assert!(harness
            .ctx
            .active_worker_recovery_nudges_sent
            .contains_key(&("worker-1".to_string(), "T-owned".to_string())));
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "in_progress");
        assert_eq!(task["assignee"], "worker-1");
    }

    #[test]
    fn active_assigned_worker_without_task_progress_reassigns_after_nudge_despite_recent_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_with_assignment_fixture(
            &brehon_root,
            chrono::Utc::now() - chrono::Duration::seconds(180),
            false,
        );
        write_worker_worktree_fixture(&brehon_root, "worker-1");
        write_worker_session_fixture(&brehon_root, "worker-2");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        mux.add_pane(make_worker_pane("worker-2"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.active_worker_recovery_nudges_sent.insert(
            ("worker-1".to_string(), "T-owned".to_string()),
            now - Duration::from_secs(90),
        );
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let recycled = recv_route(&harness.rx);
        assert_eq!(recycled.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            recycled.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-fence recovered stalled worker pane after task T-owned handoff"
        ));
        let reassigned = recv_route(&harness.rx);
        assert_eq!(
            reassigned.command.target.pane_id.as_deref(),
            Some("worker-2")
        );
        assert!(matches!(
            reassigned.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("You have been assigned recovered task T-owned: Owned task")
                    && text.contains("worker-1")
        ));
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "no-progress recovery should not emit extra same-sweep work"
        );

        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "assigned");
        assert_eq!(task["assignee"], "worker-2");
        assert!(task["recovery_note"]
            .as_str()
            .unwrap_or("")
            .contains("Automatically recovered stalled task"));
    }

    #[test]
    fn active_assigned_worker_with_task_progress_receipt_is_not_no_progress_recovered() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_with_assignment_fixture(
            &brehon_root,
            chrono::Utc::now() - chrono::Duration::seconds(180),
            true,
        );
        write_worker_worktree_fixture(&brehon_root, "worker-1");
        write_worker_session_fixture(&brehon_root, "worker-2");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        mux.add_pane(make_worker_pane("worker-2"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("worker-1".to_string(), now);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "valid task progress receipt must suppress no-progress recovery"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "in_progress");
        assert_eq!(task["assignee"], "worker-1");
    }

    #[test]
    fn stale_clean_active_assigned_worker_requeues_task_after_nudge_window() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_worktree_fixture(&brehon_root, "worker-1");

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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-fence recovered stalled worker pane after task T-owned handoff"
        ));
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "pending");
        assert_eq!(task["assignee"], serde_json::Value::Null);
        assert_eq!(task["review_owner"], serde_json::Value::Null);
        assert!(task["recovery_note"]
            .as_str()
            .unwrap_or("")
            .contains("Automatically reclaimed stalled task"));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
    }

    #[test]
    fn stale_clean_active_assigned_worker_reassigns_to_live_idle_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_worktree_fixture(&brehon_root, "worker-1");
        write_worker_session_fixture(&brehon_root, "worker-2");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        mux.add_pane(make_worker_pane("worker-2"));
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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let recycled = recv_route(&harness.rx);
        assert_eq!(recycled.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            recycled.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-fence recovered stalled worker pane after task T-owned handoff"
        ));
        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-2"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("You have been assigned recovered task T-owned: Owned task")
                    && text.contains("worker-1")
                    && text.contains("action=mine")
        ));
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "newly reassigned worker should not be treated as stale again in the same sweep"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "assigned");
        assert_eq!(task["assignee"], "worker-2");
        assert_eq!(task["review_owner"], serde_json::Value::Null);
        assert!(task["recovery_note"]
            .as_str()
            .unwrap_or("")
            .contains("Automatically recovered stalled task"));
    }

    #[test]
    fn dead_active_assigned_worker_reassigns_and_recycles_old_pane_same_sweep() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_worktree_fixture(&brehon_root, "worker-1");
        write_worker_session_fixture(&brehon_root, "worker-2");

        let mut mux = Mux::new(24, 80);
        let mut pane = make_worker_pane("worker-1");
        pane.mark_exited(Some(1));
        mux.add_pane(pane);
        mux.add_pane(make_worker_pane("worker-2"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.auto_recover_threshold = Duration::from_secs(60 * 60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let first = recv_route(&harness.rx);
        let second = recv_route(&harness.rx);
        let routed = [first, second];
        assert!(
            routed.iter().any(|route| {
                route.command.target.pane_id.as_deref() == Some("worker-1")
                    && matches!(
                        route.command.kind,
                        RuntimeCommandKind::RecyclePane { ref reason }
                            if reason
                                == "auto-fence recovered stalled worker pane after task T-owned handoff"
                    )
            }),
            "dead recovered worker should be recycled in the same sweep"
        );
        assert!(
            routed.iter().any(|route| {
                route.command.target.pane_id.as_deref() == Some("worker-2")
                    && matches!(
                        route.command.kind,
                        RuntimeCommandKind::SendPrompt { ref text, .. }
                            if text.contains("You have been assigned recovered task T-owned: Owned task")
                    )
            }),
            "replacement worker should receive the recovered assignment prompt"
        );

        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "assigned");
        assert_eq!(task["assignee"], "worker-2");
        assert_eq!(task["review_owner"], serde_json::Value::Null);
    }

    #[test]
    fn stale_clean_active_assigned_worker_skips_quarantined_and_pending_recovery_workers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_worktree_fixture(&brehon_root, "worker-1");
        write_worker_session_fixture(&brehon_root, "worker-2");
        write_worker_session_fixture(&brehon_root, "worker-3");
        write_worker_session_fixture(&brehon_root, "worker-4");
        write_taskless_prompt_blocked_health_fixture(&brehon_root, "worker-2");
        write_quarantined_worker_health_fixture(&brehon_root, "worker-3");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        mux.add_pane(make_worker_pane("worker-2"));
        mux.add_pane(make_worker_pane("worker-3"));
        mux.add_pane(make_worker_pane("worker-4"));
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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let first = recv_route(&harness.rx);
        assert_eq!(first.command.target.pane_id.as_deref(), Some("worker-2"));
        assert!(matches!(
            first.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason.contains("auto-recover prompt-blocked idle worker pane")
        ));
        let second = recv_route(&harness.rx);
        assert_eq!(second.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            second.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-fence recovered stalled worker pane after task T-owned handoff"
        ));
        let third = recv_route(&harness.rx);
        assert_eq!(third.command.target.pane_id.as_deref(), Some("worker-4"));
        assert!(matches!(
            third.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("You have been assigned recovered task T-owned: Owned task")
        ));
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "quarantined or already-recovering workers must not receive the recovered task"
        );

        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "assigned");
        assert_eq!(task["assignee"], "worker-4");
    }

    #[test]
    fn stale_clean_active_assigned_worker_skips_same_sweep_taskless_recycle_workers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_worktree_fixture(&brehon_root, "worker-1");
        write_worker_session_fixture(&brehon_root, "worker-2");
        write_worker_session_fixture(&brehon_root, "worker-3");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-2"));
        mux.add_pane(make_worker_pane("worker-1"));
        mux.add_pane(make_worker_pane("worker-3"));
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
            .last_activity
            .insert("worker-2".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let first = recv_route(&harness.rx);
        assert_eq!(first.command.target.pane_id.as_deref(), Some("worker-2"));
        assert!(matches!(
            first.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-recover idle worker pane via daemon recycle"
        ));
        let second = recv_route(&harness.rx);
        assert_eq!(second.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            second.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-fence recovered stalled worker pane after task T-owned handoff"
        ));
        let third = recv_route(&harness.rx);
        assert_eq!(third.command.target.pane_id.as_deref(), Some("worker-3"));
        assert!(matches!(
            third.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("You have been assigned recovered task T-owned: Owned task")
        ));
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "worker queued for same-sweep recycle must not be reused as the recovered assignee"
        );

        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "assigned");
        assert_eq!(task["assignee"], "worker-3");
    }

    #[test]
    fn stale_missing_worktree_active_assigned_worker_blocks_for_manual_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_session_fixture(&brehon_root, "worker-2");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        mux.add_pane(make_worker_pane("worker-2"));
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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "missing worktree must block the task instead of queueing a stale-worker reset"
        );

        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "blocked");
        assert_eq!(task["assignee"], serde_json::Value::Null);
        assert_eq!(task["review_owner"], serde_json::Value::Null);
        assert_eq!(
            task["activity"],
            crate::run::recovery::STALLED_WORKER_MANUAL_RECOVERY_ACTIVITY
        );
        assert!(task["blockers"]
            .as_str()
            .unwrap_or("")
            .contains("worker worktree is missing; manual recovery is required"));
        assert!(task["recovery_note"].as_str().unwrap_or("").contains(
            "Cleared worker ownership and blocked the task for supervisor/manual recovery"
        ));
        assert!(
            !harness
                .ctx
                .active_worker_recovery_resets_sent
                .contains_key(&("worker-1".to_string(), "T-owned".to_string())),
            "manual recovery blocking must not arm the stale-worker reset guard"
        );
    }

    #[test]
    fn stale_unmerged_active_assigned_worker_escalates_supervisor_conflict() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        let worktree = write_worker_worktree_fixture(&brehon_root, "worker-1");

        std::fs::write(worktree.join("shared.txt"), "base\n").expect("base file");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["add", "shared.txt"])
            .status()
            .expect("git add base");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["commit", "-m", "base"])
            .status()
            .expect("git commit base");
        assert!(status.success());

        let default_branch_output = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["branch", "--show-current"])
            .output()
            .expect("git branch --show-current");
        assert!(default_branch_output.status.success());
        let default_branch = String::from_utf8_lossy(&default_branch_output.stdout)
            .trim()
            .to_string();

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["checkout", "-b", "other"])
            .status()
            .expect("git checkout other");
        assert!(status.success());
        std::fs::write(worktree.join("shared.txt"), "other\n").expect("other file");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["add", "shared.txt"])
            .status()
            .expect("git add other");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["commit", "-m", "other"])
            .status()
            .expect("git commit other");
        assert!(status.success());

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["checkout", &default_branch])
            .status()
            .expect("git checkout default branch");
        assert!(status.success());
        std::fs::write(worktree.join("shared.txt"), "main\n").expect("main file");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["add", "shared.txt"])
            .status()
            .expect("git add main");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["commit", "-m", "main"])
            .status()
            .expect("git commit main");
        assert!(status.success());

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["merge", "other"])
            .status()
            .expect("git merge other");
        assert!(!status.success(), "merge should conflict");

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join("T-owned.json"),
            serde_json::json!({
                "task_id": "T-owned",
                "title": "Owned task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1",
                "review_owner": "worker-1",
                "merge_target": "epic/test",
                "latest_commit": "abc123"
            })
            .to_string(),
        )
        .expect("updated task fixture");

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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("worker-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::RecyclePane { ref reason }
                if reason == "auto-fence recovered stalled worker pane after task T-owned handoff"
        ));
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(task["status"], "changes_requested");
        assert_eq!(task["assignee"], serde_json::Value::Null);
        assert_eq!(task["review_owner"], serde_json::Value::Null);
        assert_eq!(task["activity"], "integration_conflict");
        assert_eq!(task["integration_conflict"]["owner"], "supervisor");
        assert_eq!(task["integration_conflict"]["source"], "worker_unmerged");
        assert_eq!(
            task["integration_conflict"]["conflicting_files"][0],
            "shared.txt"
        );
    }

    #[test]
    fn prompt_blocked_active_reviewer_blocks_task_when_reset_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "reviewer-1", "T-review");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        mux.get_mut("reviewer-1")
            .expect("reviewer pane")
            .set_review_context(brehon_mux::ReviewContextSnapshot {
                review_id: "REV-review".to_string(),
                task_id: "T-review".to_string(),
                round: 1,
                panel_total: 1,
                panel_done: 0,
                verdict: None,
                score: None,
                findings_summary: None,
                updated_at: Instant::now(),
            });
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "blocking the task after failed reviewer recovery should clear health marker"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-review.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            task.get("activity").and_then(|value| value.as_str()),
            Some("prompt-blocked recovery failed")
        );
        assert!(task
            .get("blockers")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.contains("runtime command router unavailable")));
    }

    #[test]
    fn prompt_blocked_active_reviewer_terminal_task_failure_converges_to_terminal_marker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_terminal_task_fixture(&brehon_root, "T-review", "merged");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "reviewer-1", "T-review");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        mux.get_mut("reviewer-1")
            .expect("reviewer pane")
            .set_review_context(brehon_mux::ReviewContextSnapshot {
                review_id: "REV-review".to_string(),
                task_id: "T-review".to_string(),
                round: 1,
                panel_total: 1,
                panel_done: 0,
                verdict: None,
                score: None,
                findings_summary: None,
                updated_at: Instant::now(),
            });
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let first_marker =
            std::fs::read_to_string(&health_path).expect("terminal prompt-blocked marker");
        let first_marker_json =
            serde_json::from_str::<serde_json::Value>(&first_marker).expect("marker json");
        assert_eq!(
            first_marker_json
                .get("reason")
                .and_then(|value| value.as_str()),
            Some("prompt_blocked_recovery_failed")
        );
        assert!(first_marker_json
            .get("error")
            .and_then(|value| value.as_str())
            .is_some_and(|value| {
                value.contains("could not mark task T-review blocked")
                    && value.contains("terminal task T-review")
            }));
        let terminal_task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-review.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            terminal_task.get("status").and_then(|value| value.as_str()),
            Some("merged")
        );
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_marker = std::fs::read_to_string(&health_path).expect("marker after resweep");
        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_marker, second_marker,
            "terminal task prompt-blocked reviewer failures should converge to a one-shot terminal marker"
        );
        assert_eq!(
            first_event_count, second_event_count,
            "terminal task prompt-blocked reviewer failures should not emit duplicate dashboard events after fallback marker convergence"
        );
    }

    #[test]
    fn prompt_blocked_active_reviewer_marker_write_failure_is_suppressed_in_memory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_terminal_task_fixture(&brehon_root, "T-review", "merged");
        write_invalid_agent_health_path_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        mux.get_mut("reviewer-1")
            .expect("reviewer pane")
            .set_review_context(brehon_mux::ReviewContextSnapshot {
                review_id: "REV-review".to_string(),
                task_id: "T-review".to_string(),
                round: 1,
                panel_total: 1,
                panel_done: 0,
                verdict: None,
                score: None,
                findings_summary: None,
                updated_at: Instant::now(),
            });
        apply_prompt_blocked_runtime_state_with_task(&mut mux, "reviewer-1", Some("T-review"));
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            harness
                .ctx
                .prompt_blocked_recovery_failed_panes
                .contains("reviewer-1"),
            "live blocked panes should be suppressed in memory when the recovery-failed marker cannot be persisted"
        );
        let merged_task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-review.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            merged_task.get("status").and_then(|value| value.as_str()),
            Some("merged")
        );
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_event_count, second_event_count,
            "once in-memory suppression is recorded for a live blocked reviewer, later stall sweeps should not retry the same failed recovery"
        );
    }

    #[test]
    fn prompt_blocked_active_worker_blocks_task_when_reset_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-owned");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "blocking the task after failed recovery should clear health marker"
        );
        let task = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-owned.json"),
            )
            .expect("task file"),
        )
        .expect("task json");
        assert_eq!(
            task.get("status").and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            task.get("activity").and_then(|value| value.as_str()),
            Some("prompt-blocked recovery failed")
        );
        assert!(task
            .get("blockers")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.contains("runtime command router unavailable")));
        assert!(!harness
            .ctx
            .last_worker_context_reset
            .contains_key("worker-1"));
    }

    #[test]
    fn prompt_blocked_recovery_failure_is_not_retried_for_same_blocked_pane() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_active_assigned_task_fixture(&brehon_root);
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path =
            write_prompt_blocked_worker_health_fixture(&brehon_root, "worker-1", "T-owned");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        apply_prompt_blocked_runtime_state(&mut mux, "worker-1", "T-owned");
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(
            !health_path.exists(),
            "blocking the task after failed recovery should clear health marker"
        );
        let task_path = brehon_root
            .join("runtime")
            .join("tasks")
            .join("T-owned.json");
        let first_task = std::fs::read_to_string(&task_path).expect("task file after first sweep");
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_task =
            std::fs::read_to_string(&task_path).expect("task file after second sweep");
        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_task, second_task,
            "once prompt-blocked recovery marks the task blocked, later stall sweeps should not rewrite the task file for the same pane"
        );
        assert_eq!(
            first_event_count, second_event_count,
            "once prompt-blocked recovery marks the task blocked, later stall sweeps should not emit duplicate recovery-failure dashboard events"
        );
    }

    #[test]
    fn taskless_prompt_blocked_supervisor_failure_is_marked_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let health_path =
            write_taskless_prompt_blocked_health_fixture(&brehon_root, "claude-supervisor");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        apply_taskless_prompt_blocked_runtime_state(&mut mux, "claude-supervisor");
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let first_marker = std::fs::read_to_string(&health_path).expect("terminal failure marker");
        let marker = serde_json::from_str::<serde_json::Value>(&first_marker).expect("marker json");
        assert_eq!(
            marker.get("reason").and_then(|value| value.as_str()),
            Some("prompt_blocked_recovery_failed")
        );
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_marker = std::fs::read_to_string(&health_path).expect("marker after resweep");
        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_marker,
            second_marker,
            "taskless prompt-blocked supervisor failures should keep a one-shot terminal marker instead of rewriting it every stall sweep"
        );
        assert_eq!(
            first_event_count, second_event_count,
            "taskless prompt-blocked supervisor failures should not emit duplicate dashboard events after the terminal marker is recorded"
        );
    }

    #[test]
    fn taskless_prompt_blocked_worker_failure_is_marked_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_inactive_task_fixture(&brehon_root, "T-unrelated");
        write_worker_session_fixture(&brehon_root, "worker-1");
        let health_path = write_taskless_prompt_blocked_health_fixture(&brehon_root, "worker-1");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        apply_taskless_prompt_blocked_runtime_state(&mut mux, "worker-1");
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.runtime_command_router = None;
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let first_marker =
            std::fs::read_to_string(&health_path).expect("worker terminal failure marker");
        let marker = serde_json::from_str::<serde_json::Value>(&first_marker).expect("marker");
        assert_eq!(
            marker.get("reason").and_then(|value| value.as_str()),
            Some("prompt_blocked_recovery_failed")
        );
        let first_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert!(
            harness.ctx.dashboard_data.lock().tasks.is_empty(),
            "marker-only taskless recovery should not trigger an unrelated task refresh"
        );

        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let second_marker = std::fs::read_to_string(&health_path).expect("marker after resweep");
        let second_event_count = harness.ctx.dashboard_data.lock().events.len();
        assert_eq!(
            first_marker, second_marker,
            "taskless prompt-blocked worker failures should keep a one-shot terminal marker instead of retrying the same failed recovery"
        );
        assert_eq!(
            first_event_count, second_event_count,
            "taskless prompt-blocked worker failures should not emit duplicate dashboard events after the terminal marker is recorded"
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
            Instant::now(),
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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

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
            Instant::now(),
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
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("reviewer-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("Review-obligation nudge")
                    && text.contains("action=review_status task_id=T-review review_id=REV-review")
        ));
        assert!(harness
            .ctx
            .review_obligation_notifications_sent
            .contains_key(&(
                "reviewer-1".to_string(),
                "T-review".to_string(),
                "REV-review".to_string()
            )));
    }

    #[test]
    fn stale_live_reviewer_with_uncertain_delivery_gets_review_request_resent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture_with_uncertain_delivery(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("reviewer-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text == &canonical_review_request_prompt_fixture()
        ));
        assert!(harness.ctx.review_obligation_resends_sent.contains_key(&(
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
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.review_obligation_notifications_sent.insert(
            (
                "reviewer-1".to_string(),
                "T-review".to_string(),
                "REV-review".to_string(),
            ),
            now - Duration::from_secs(120),
        );
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

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
    fn stale_live_reviewer_obligation_resets_when_idle_exceeds_reset_threshold() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(3);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(routed.command.target.pane_id.as_deref(), Some("reviewer-1"));
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason }
                if reason == "auto-recover idle reviewer pane with pending review obligation"
        ));
        assert_eq!(harness.ctx.pending_runtime_commands.len(), 1);
    }

    #[test]
    fn missing_reviewer_obligation_hard_failure_notifies_supervisor_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routes = recv_available_routes(&harness.rx);
        assert_eq!(
            routes.len(),
            1,
            "expected one supervisor hard-failure prompt"
        );
        assert_eq!(
            routes[0].command.target.pane_id.as_deref(),
            Some("claude-supervisor")
        );
        assert!(matches!(
            routes[0].command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("reviewer-1")
                    && text.contains("hard failure")
                    && text.contains("T-review")
                    && text.contains("REV-review")
        ));
        assert!(harness.ctx.review_obligation_failures_reported.contains(&(
            "reviewer-1".to_string(),
            "T-review".to_string(),
            "REV-review".to_string()
        )));

        drain_runtime_commands(&mut harness);
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);
        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "already-reported hard failure should not loop"
        );
    }

    #[test]
    fn wrong_kind_reviewer_obligation_hard_failure_notifies_supervisor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        mux.add_pane(make_worker_pane("reviewer-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routes = recv_available_routes(&harness.rx);
        assert_eq!(
            routes.len(),
            1,
            "expected one supervisor hard-failure prompt"
        );
        assert_eq!(
            routes[0].command.target.pane_id.as_deref(),
            Some("claude-supervisor")
        );
        assert!(matches!(
            routes[0].command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("reviewer-1")
                    && text.contains("hard failure")
                    && text.contains("not a reviewer pane")
        ));
        assert!(harness.ctx.review_obligation_failures_reported.contains(&(
            "reviewer-1".to_string(),
            "T-review".to_string(),
            "REV-review".to_string()
        )));
    }

    #[test]
    fn nudge_delivery_failure_reports_supervisor_hard_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        let mut harness = harness_with_selective_router(
            mux,
            true,
            vec![TestRouteRejection::SendPrompt("reviewer-1".to_string())],
        );
        let now = Instant::now();
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routes = recv_available_routes(&harness.rx);
        assert_eq!(
            routes.len(),
            2,
            "expected rejected nudge and supervisor escalation"
        );
        assert!(routes.iter().any(|route| {
            route.command.target.pane_id.as_deref() == Some("reviewer-1")
                && matches!(
                    route.command.kind,
                    RuntimeCommandKind::SendPrompt { ref text, .. }
                        if text.contains("Review-obligation nudge")
                )
        }));
        assert!(routes.iter().any(|route| {
            route.command.target.pane_id.as_deref() == Some("claude-supervisor")
                && matches!(
                    route.command.kind,
                    RuntimeCommandKind::SendPrompt { ref text, .. }
                        if text.contains("reviewer-1")
                            && text.contains("hard failure")
                            && text.contains("recovery nudge could not be delivered")
                )
        }));
        assert!(harness.ctx.review_obligation_failures_reported.contains(&(
            "reviewer-1".to_string(),
            "T-review".to_string(),
            "REV-review".to_string()
        )));
    }

    #[test]
    fn reset_queue_failure_reports_review_obligation_hard_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        mux.add_pane(make_reviewer_pane("reviewer-1"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.review_obligation_notifications_sent.insert(
            (
                "reviewer-1".to_string(),
                "T-review".to_string(),
                "REV-review".to_string(),
            ),
            now - Duration::from_secs(120),
        );
        harness
            .ctx
            .last_activity
            .insert("reviewer-1".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());
        harness.ctx.runtime_command_router = None;

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(harness.ctx.review_obligation_failures_reported.contains(&(
            "reviewer-1".to_string(),
            "T-review".to_string(),
            "REV-review".to_string()
        )));
        let dashboard = harness.ctx.dashboard_data.lock();
        assert!(dashboard.events.iter().any(|event| {
            event.description
                == "reported review-obligation hard failure for reviewer reviewer-1 on T-review / REV-review"
        }));
    }

    #[test]
    fn phase_1_gate_multi_reviewer_simulation_prevents_silent_idle() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_multi_reviewer_phase_gate_fixture(&brehon_root);

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        mux.add_pane(make_reviewer_pane("reviewer-resend"));
        mux.add_pane(make_reviewer_pane("reviewer-reset"));
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.review_obligation_nudge_threshold = Duration::from_secs(1);
        harness.ctx.review_obligation_reset_threshold = Duration::from_secs(60);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness
            .ctx
            .last_activity
            .insert("reviewer-resend".to_string(), now - Duration::from_secs(5));
        harness
            .ctx
            .last_activity
            .insert("reviewer-reset".to_string(), now - Duration::from_secs(5));
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routes = recv_available_routes(&harness.rx);
        assert_eq!(
            routes.len(),
            3,
            "expected resend, nudge, and hard-failure actions"
        );
        let mut saw_resend = false;
        let mut saw_nudge = false;
        let mut saw_hard_failure = false;
        for route in &routes {
            match &route.command.kind {
                RuntimeCommandKind::SendPrompt { text, .. }
                    if route.command.target.pane_id.as_deref() == Some("reviewer-resend")
                        && text == "Canonical resend prompt for reviewer-resend" =>
                {
                    saw_resend = true;
                }
                RuntimeCommandKind::SendPrompt { text, .. }
                    if route.command.target.pane_id.as_deref() == Some("reviewer-reset")
                        && text.contains("Review-obligation nudge")
                        && text.contains("REV-phase-gate") =>
                {
                    saw_nudge = true;
                }
                RuntimeCommandKind::SendPrompt { text, .. }
                    if route.command.target.pane_id.as_deref() == Some("claude-supervisor")
                        && text.contains("reviewer-missing")
                        && text.contains("hard failure") =>
                {
                    saw_hard_failure = true;
                }
                other => panic!("unexpected routed command in phase-gate simulation: {other:?}"),
            }
        }
        assert!(saw_resend, "uncertain delivery reviewer should get resend");
        assert!(saw_nudge, "idle reviewer should get a recovery nudge first");
        assert!(
            saw_hard_failure,
            "missing reviewer should produce a supervisor-visible hard failure"
        );
        assert!(harness.ctx.review_obligation_resends_sent.contains_key(&(
            "reviewer-resend".to_string(),
            "T-phase-gate".to_string(),
            "REV-phase-gate".to_string()
        )));
        assert!(harness
            .ctx
            .review_obligation_notifications_sent
            .contains_key(&(
                "reviewer-reset".to_string(),
                "T-phase-gate".to_string(),
                "REV-phase-gate".to_string()
            )));
        assert!(harness.ctx.review_obligation_failures_reported.contains(&(
            "reviewer-missing".to_string(),
            "T-phase-gate".to_string(),
            "REV-phase-gate".to_string()
        )));

        drain_runtime_commands(&mut harness);
        harness.ctx.review_obligation_notifications_sent.insert(
            (
                "reviewer-reset".to_string(),
                "T-phase-gate".to_string(),
                "REV-phase-gate".to_string(),
            ),
            Instant::now() - Duration::from_secs(120),
        );
        harness.ctx.last_activity.insert(
            "reviewer-reset".to_string(),
            Instant::now() - Duration::from_secs(5),
        );
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routes = recv_available_routes(&harness.rx);
        assert_eq!(routes.len(), 1, "expected only the queued reviewer reset");
        assert_eq!(
            routes[0].command.target.pane_id.as_deref(),
            Some("reviewer-reset")
        );
        assert!(matches!(
            routes[0].command.kind,
            RuntimeCommandKind::ResetPane { ref reason }
                if reason == "auto-recover idle reviewer pane with pending review obligation"
        ));
        drain_runtime_commands(&mut harness);
        let ack_path = brehon_root
            .join("runtime")
            .join("reviewer-reset-acks")
            .join("T-phase-gate--REV-phase-gate--reviewer-reset.json");
        assert!(ack_path.exists(), "reset simulation should persist an ack");
    }

    #[test]
    fn completed_review_prunes_review_obligation_tracking_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        write_review_obligation_fixture(&brehon_root);

        let runtime_dir = brehon_root.join("runtime");
        std::fs::write(
            runtime_dir.join("tasks").join("T-review.json"),
            serde_json::json!({
                "task_id": "T-review",
                "title": "Pending review task",
                "status": "review_ready",
                "task_type": "task"
            })
            .to_string(),
        )
        .expect("updated task file");
        std::fs::write(
            runtime_dir
                .join("reviews")
                .join("T-review")
                .join("state.json"),
            serde_json::json!({
                "task_id": "T-review",
                "status": "approved",
                "current_round": 1,
                "current_review_id": "REV-review",
                "max_rounds": 3,
                "panel_id": "primary",
                "panel": ["reviewer-1"],
                "submissions_received": ["reviewer-1"],
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("updated review state");

        let mux = Mux::new(24, 80);
        let mut harness = harness_with_mux(mux);
        let now = Instant::now();
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = now - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());
        harness.ctx.review_obligation_notifications_sent.insert(
            (
                "reviewer-1".to_string(),
                "T-review".to_string(),
                "REV-review".to_string(),
            ),
            now,
        );
        harness.ctx.review_obligation_resends_sent.insert(
            (
                "reviewer-1".to_string(),
                "T-review".to_string(),
                "REV-review".to_string(),
            ),
            now,
        );
        harness.ctx.review_obligation_failures_reported.insert((
            "reviewer-1".to_string(),
            "T-review".to_string(),
            "REV-review".to_string(),
        ));

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        assert!(harness.ctx.review_obligation_notifications_sent.is_empty());
        assert!(harness.ctx.review_obligation_resends_sent.is_empty());
        assert!(harness.ctx.review_obligation_failures_reported.is_empty());
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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

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
    fn quarantined_supervisor_routes_context_reset_and_clears_health() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        std::fs::create_dir_all(runtime_dir.join("sessions")).expect("sessions dir");
        std::fs::create_dir_all(runtime_dir.join("agent-health")).expect("health dir");

        std::fs::write(
            runtime_dir.join("sessions").join("claude-supervisor.json"),
            serde_json::json!({
                "name": "claude-supervisor",
                "role": "supervisor",
                "session_id": "session-supervisor",
                "last_seen_at": chrono::Utc::now().to_rfc3339()
            })
            .to_string(),
        )
        .expect("session file");
        let health_path = runtime_dir
            .join("agent-health")
            .join("claude-supervisor.json");
        std::fs::write(
            &health_path,
            serde_json::json!({
                "agent": "claude-supervisor",
                "status": "unavailable",
                "reason": "quota_exhausted"
            })
            .to_string(),
        )
        .expect("health file");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.stall_check_interval = Duration::ZERO;
        harness.ctx.last_stall_check = Instant::now() - Duration::from_secs(60);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::stall_handling::detect_and_handle_stalls(&mut harness.ctx);

        let routed = recv_route(&harness.rx);
        assert_eq!(
            routed.command.target.pane_id.as_deref(),
            Some("claude-supervisor")
        );
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::ResetPane { ref reason }
                if reason == "auto-recover quarantined supervisor pane via daemon reset"
        ));

        drain_runtime_commands(&mut harness);

        assert!(
            !health_path.exists(),
            "successful reset should clear stale supervisor quarantine marker"
        );
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
    }

    #[test]
    fn queued_prompt_to_quarantined_supervisor_is_deferred_not_dead_lettered() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        let queue_dir = runtime_dir.join("prompt-queue");
        std::fs::create_dir_all(&queue_dir).expect("queue dir");
        std::fs::create_dir_all(runtime_dir.join("agent-health")).expect("health dir");
        let prompt_path = queue_dir.join("000.prompt");
        std::fs::write(
            &prompt_path,
            serde_json::json!({
                "target": "claude-supervisor",
                "from": "review-coordinator",
                "message": "Review complete for task T-approved\nTask approved (awaiting merge-target integration). Run task action=integrate id=T-approved",
                "prompt_id": "prompt-approved"
            })
            .to_string(),
        )
        .expect("prompt file");
        std::fs::write(
            runtime_dir
                .join("agent-health")
                .join("claude-supervisor.json"),
            serde_json::json!({
                "agent": "claude-supervisor",
                "status": "unavailable",
                "reason": "prompt_blocked"
            })
            .to_string(),
        )
        .expect("health file");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        apply_taskless_prompt_blocked_runtime_state(&mut mux, "claude-supervisor");
        let mut harness = harness_with_mux(mux);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::prompt_delivery::deliver_pending_prompts(&mut harness.ctx, &brehon_root);

        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "quarantined supervisor prompt should not be routed until reset succeeds"
        );
        assert!(
            prompt_path.exists(),
            "critical supervisor prompt must stay durable while supervisor is quarantined"
        );
        assert!(
            crate::run::recovery::prompt_retry_meta_path(&prompt_path).exists(),
            "deferred supervisor prompt should record retry metadata"
        );
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 1);
        let dead_letter_count = std::fs::read_dir(runtime_dir.join("prompt-dead-letter"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(dead_letter_count, 0);
    }

    #[test]
    fn queued_prompt_to_ready_supervisor_clears_stale_prompt_blocked_marker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        let queue_dir = runtime_dir.join("prompt-queue");
        let health_dir = runtime_dir.join("agent-health");
        std::fs::create_dir_all(&queue_dir).expect("queue dir");
        std::fs::create_dir_all(&health_dir).expect("health dir");
        let prompt_path = queue_dir.join("000.prompt");
        std::fs::write(
            &prompt_path,
            serde_json::json!({
                "target": "claude-supervisor",
                "from": "review-coordinator",
                "message": "Review complete for task T-approved\nTask approved (awaiting merge-target integration). Run task action=integrate id=T-approved",
                "prompt_id": "prompt-approved"
            })
            .to_string(),
        )
        .expect("prompt file");
        let health_path = health_dir.join("claude-supervisor.json");
        std::fs::write(
            &health_path,
            serde_json::json!({
                "agent": "claude-supervisor",
                "status": "unavailable",
                "reason": "prompt_blocked"
            })
            .to_string(),
        )
        .expect("health file");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        let ready_event = brehon_types::RuntimeEvent::new(
            brehon_types::RuntimeEventMeta::new(
                "session",
                "claude-supervisor",
                0,
                brehon_types::RuntimeSource::Headless,
                1,
            ),
            brehon_types::RuntimeEventKind::PaneStateChanged(brehon_types::PaneStateChangedEvent {
                previous: None,
                current: brehon_types::RuntimePaneState::Ready,
                reason: Some("state machine ready".to_string()),
                blocked: None,
            }),
        );
        assert!(
            mux.apply_terminal_host_runtime_event(&ready_event)
                .expect("apply ready event"),
            "ready event should mark supervisor pane ready"
        );
        let mut harness = harness_with_mux(mux);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::prompt_delivery::deliver_pending_prompts(&mut harness.ctx, &brehon_root);

        let routed = recv_route(&harness.rx);
        assert_eq!(
            routed.command.target.pane_id.as_deref(),
            Some("claude-supervisor")
        );
        assert!(matches!(
            routed.command.kind,
            RuntimeCommandKind::SendPrompt { ref text, .. }
                if text.contains("task action=integrate id=T-approved")
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
            !health_path.exists(),
            "stale ready-supervisor quarantine marker should be cleared"
        );
        assert!(
            !prompt_path.exists(),
            "successful delivery should remove the queued supervisor prompt"
        );
        assert_eq!(harness.ctx.mux.pending_delayed_prompt_count(), 0);
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
        mux.add_pane(make_terminal_worker_pane("worker-1"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

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
                .join(format!("{}.json", sanitize_prompt_key("prompt-1")))
                .exists(),
            "successful daemon delivery writes prompt ack"
        );
    }

    #[test]
    fn queued_prompt_for_missing_target_is_dead_lettered_without_retry_loop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        let queue_dir = runtime_dir.join("prompt-queue");
        std::fs::create_dir_all(&queue_dir).expect("queue dir");
        let prompt_path = queue_dir.join("000.prompt");
        std::fs::write(
            &prompt_path,
            serde_json::json!({
                "target": "stale-worker",
                "from": "supervisor",
                "message": "this prompt belongs to a previous runtime",
                "prompt_id": "prompt-stale"
            })
            .to_string(),
        )
        .expect("prompt file");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("live-worker"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::prompt_delivery::deliver_pending_prompts(&mut harness.ctx, &brehon_root);

        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "stale prompt should not route to the runtime"
        );
        assert!(!prompt_path.exists(), "stale prompt should be moved away");
        assert!(
            !crate::run::recovery::prompt_retry_meta_path(&prompt_path).exists(),
            "stale prompt should not leave retry metadata behind"
        );
        let dead_letters = std::fs::read_dir(runtime_dir.join("prompt-dead-letter"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(dead_letters, 1);
    }

    #[test]
    fn queued_prompt_sweep_budget_limits_stale_prompt_churn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let runtime_dir = brehon_root.join("runtime");
        let queue_dir = runtime_dir.join("prompt-queue");
        std::fs::create_dir_all(&queue_dir).expect("queue dir");
        let prompt_count = crate::run::prompt_delivery::MAX_PROMPT_DELIVERY_ATTEMPTS_PER_SWEEP + 1;
        for idx in 0..prompt_count {
            std::fs::write(
                queue_dir.join(format!("{idx:03}.prompt")),
                serde_json::json!({
                    "target": format!("stale-worker-{idx}"),
                    "from": "supervisor",
                    "message": "this prompt belongs to a previous runtime",
                    "prompt_id": format!("prompt-stale-{idx}")
                })
                .to_string(),
            )
            .expect("prompt file");
        }

        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("live-worker"));
        let mut harness = harness_with_mux(mux);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

        crate::run::prompt_delivery::deliver_pending_prompts(&mut harness.ctx, &brehon_root);

        assert!(
            harness.rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "stale prompts should not route to the runtime"
        );
        let remaining_prompts = std::fs::read_dir(&queue_dir)
            .expect("queue entries")
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "prompt"))
            .count();
        assert_eq!(
            remaining_prompts, 1,
            "one prompt should remain for the next sweep"
        );
        let dead_letters = std::fs::read_dir(runtime_dir.join("prompt-dead-letter"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(
            dead_letters,
            crate::run::prompt_delivery::MAX_PROMPT_DELIVERY_ATTEMPTS_PER_SWEEP
        );
    }

    #[test]
    fn composer_submission_routes_through_durable_prompt_queue() {
        let temp = tempfile::tempdir().expect("tempdir");
        let brehon_root = temp.path().join(".brehon");
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("supervisor"));
        let mut harness = harness_with_host_owned_mux(mux);
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

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
        harness.ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());

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

    /// Fix C firewall: a panic inside a per-tick body must NOT unwind out of
    /// the run loop and end the unattended session. This mirrors the exact
    /// `catch_unwind(AssertUnwindSafe(..))` structure used around the
    /// stall/budget/supervisor-reset ticks in [`run`]: a panicking tick on
    /// one iteration is caught, logged, and the loop proceeds to the next
    /// iteration and exits cleanly.
    #[test]
    fn panicking_tick_does_not_tear_down_loop() {
        let shutdown = std::sync::atomic::AtomicBool::new(false);
        // Counts ticks that ran to completion AFTER a panicking one, proving
        // the loop survived the panic rather than unwinding out.
        let mut completed_after_panic = 0_u32;
        let mut iteration = 0_u32;

        // Drive the same shape as `run`: a `while !shutdown` loop whose tick
        // body is wrapped in the panic firewall.
        while !shutdown.load(Ordering::Relaxed) {
            iteration += 1;

            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Iteration 1 panics (simulating a bad untrusted-event tick);
                // later iterations do real "work".
                if iteration == 1 {
                    panic!("simulated tick panic");
                }
                completed_after_panic += 1;
            }))
            .is_err();

            assert_eq!(
                caught,
                iteration == 1,
                "only the first (panicking) iteration should be caught"
            );

            // Independent latched stop, like `ctx.shutdown`: stop after a few
            // iterations so the test terminates deterministically.
            if iteration >= 3 {
                shutdown.store(true, Ordering::Relaxed);
            }
        }

        // The loop kept going past the panicking iteration and ran the
        // subsequent ticks to completion.
        assert_eq!(iteration, 3, "loop must survive the panic and keep ticking");
        assert_eq!(
            completed_after_panic, 2,
            "ticks after the panicking one must still run to completion"
        );
    }
}
