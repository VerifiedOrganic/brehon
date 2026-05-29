//! TUI run loop.
//! Mux-owned runs use a split layout with an always-visible supervisor pane.
//! Host-owned terminal runs keep the Brehon dashboard full-width while runtime
//! state and controls route through the daemon.
//!
//! Layout (Workers group active):
//! ```text
//! ┌─ Left ─────────────────────────────────┬─ Right (supervisor) ──────────┐
//! │ [ Workers (3) ] [ Reviewers (2) ]       │  supervisor [opencode]        │
//! │  worker-1  worker-2  worker-3           │                               │
//! ├─────────────────────────────────────────┤                               │
//! │                                         │                               │
//! │  Selected worker pane content           │  Supervisor pane (always on)  │
//! │                                         │                               │
//! ├─────────────────────────────────────────┴───────────────────────────────┤
//! │ C-q:Quit  C-]:Next  S-Tab:Prev  Click:Switch │ 5 panes │ worker-1      │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Layout (Reviewers group active):
//! ```text
//! ┌─ Left ─────────────────────────────────┬─ Right (supervisor) ──────────┐
//! │ [ Workers (3) ] [ Reviewers (2) ]       │  supervisor [opencode]        │
//! │ [ codex (2) ] [ claude-code (1) ]       │                               │
//! │  codex-reviewer-1  codex-reviewer-2     │                               │
//! ├─────────────────────────────────────────┤                               │
//! │                                         │                               │
//! │  Selected reviewer pane content         │  Supervisor pane (always on)  │
//! │                                         │                               │
//! ├─────────────────────────────────────────┴───────────────────────────────┤
//! │ status bar                                                              │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```

mod advisors;
mod automation;
mod composer;
mod confirmed_state;
mod crash_detection;
mod dashboard;
mod event_loop;
mod feedback_detail;
mod gateway_prompts;
mod ghostty_widget;
mod helpers;
mod input;
mod key_handling;
mod keybind_overlay;
mod layout;
mod prompt_delivery;
mod proof_detail;
mod recovery;
mod refresh;
mod rendering;
pub mod research;
mod reviewer_selection;
mod run_state_detail;
mod self_improvement;
mod session;
mod stall_handling;
mod task_detail;
mod task_scope_summary;
mod terminal_guard;
mod types;

pub use automation::RuntimeAutomationHarness;
pub use types::{AgentInfo, DashboardData, EventInfo, ReviewerPanel, TaskInfo};

use gateway_prompts::*;
use refresh::*;
use reviewer_selection::*;
use self_improvement::*;
use types::*;

// Re-exported only for the nested `#[cfg(test)] mod tests` block below.
// The outer `run_tui_with_panels` fn delegates to `event_loop::run`, so these
// symbols are not used by non-test code.
#[cfg(test)]
#[allow(unused_imports)]
use crash_detection::*;
#[cfg(test)]
#[allow(unused_imports)]
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
#[cfg(test)]
#[allow(unused_imports)]
use dashboard::*;
#[cfg(test)]
#[allow(unused_imports)]
use helpers::*;
#[cfg(test)]
#[allow(unused_imports)]
use input::*;
#[cfg(test)]
#[allow(unused_imports)]
use key_handling::*;
#[cfg(test)]
#[allow(unused_imports)]
use keybind_overlay::*;
#[cfg(test)]
#[allow(unused_imports)]
use layout::*;
#[cfg(test)]
#[allow(unused_imports)]
use ratatui::style::Color;
#[cfg(test)]
#[allow(unused_imports)]
use recovery::*;
#[cfg(test)]
#[allow(unused_imports)]
use rendering::*;
#[cfg(test)]
#[allow(unused_imports)]
use session::*;
#[cfg(test)]
#[allow(unused_imports)]
use task_detail::*;

// ── External imports for the main loop ──────────────────────────────────────

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use brehon_mux::{Mux, PaneKind};
use brehon_ports::RuntimeCommandRouter;
use brehon_types::config::{OrchestrationConfig, WorkerIdleBehavior};

use terminal_guard::{
    enter_dashboard_terminal_session, restore_terminal_session, TerminalSessionGuard,
};

// ── Main loop ───────────────────────────────────────────────────────────────

/// Run the TUI with default settings (no reviewer panels, empty dashboard).
/// Build a `ProjectConfigLoader` that always returns `None`. Used by callers
/// that never reach research/config-dependent code paths (dashboards, demos,
/// internal test helpers).
pub fn no_project_config_loader() -> research::ProjectConfigLoader {
    Arc::new(|_| None)
}

pub fn run_tui(shutdown: Arc<AtomicBool>, mux: Mux, rt: tokio::runtime::Handle) -> io::Result<()> {
    let dashboard = Arc::new(std::sync::Mutex::new(DashboardData::default()));
    run_tui_with_panels(
        shutdown,
        mux,
        rt,
        &[],
        dashboard,
        OrchestrationConfig {
            max_active_workers: 1,
            worktree_isolation: true,
            branch_prefix: "brehon/".into(),
            auto_cleanup_worktrees: true,
            worker_idle_behavior: WorkerIdleBehavior::Wait,
            allow_mutating_idle_work: false,
            self_improve_tasks: Vec::new(),
            spawn_workers: None,
            drain_timeout_secs: None,
            worktree_root: None,
        },
    )
}

/// Run the TUI with explicit reviewer panel groupings.
///
/// If `reviewer_panels` is empty, all reviewer panes appear as a flat list.
/// `dashboard_data` is shared with an optional background refresh task.
pub fn run_tui_with_panels(
    shutdown: Arc<AtomicBool>,
    mux: Mux,
    rt: tokio::runtime::Handle,
    reviewer_panels: &[ReviewerPanel],
    dashboard_data: Arc<std::sync::Mutex<DashboardData>>,
    orchestration: OrchestrationConfig,
) -> io::Result<()> {
    run_tui_with_panels_and_runtime_commands(
        shutdown,
        mux,
        rt,
        reviewer_panels,
        dashboard_data,
        orchestration,
        None,
        None,
        None,
        false,
        false,
        no_project_config_loader(),
    )
}

/// Run only the Brehon dashboard/control surface.
///
/// This is used by daemon-backed runtime status inspection without spawning the
/// normal embedded mux panes.
pub fn run_dashboard_tui(
    shutdown: Arc<AtomicBool>,
    rt: tokio::runtime::Handle,
    brehon_root: std::path::PathBuf,
) -> io::Result<()> {
    let dashboard = Arc::new(std::sync::Mutex::new(DashboardData {
        brehon_root: Some(brehon_root),
        ..Default::default()
    }));
    run_tui_with_panels_and_runtime_commands(
        shutdown,
        Mux::new(24, 80),
        rt,
        &[],
        dashboard,
        OrchestrationConfig {
            max_active_workers: 1,
            worktree_isolation: true,
            branch_prefix: "brehon/".into(),
            auto_cleanup_worktrees: true,
            worker_idle_behavior: WorkerIdleBehavior::Wait,
            allow_mutating_idle_work: false,
            self_improve_tasks: Vec::new(),
            spawn_workers: None,
            drain_timeout_secs: None,
            worktree_root: None,
        },
        None,
        None,
        None,
        true,
        false,
        no_project_config_loader(),
    )
}

/// Run the TUI with a runtime command receiver owned by the mux event loop.
pub fn run_tui_with_panels_and_runtime_commands(
    shutdown: Arc<AtomicBool>,
    mux: Mux,
    rt: tokio::runtime::Handle,
    reviewer_panels: &[ReviewerPanel],
    dashboard_data: Arc<std::sync::Mutex<DashboardData>>,
    orchestration: OrchestrationConfig,
    runtime_command_rx: Option<brehon_mux::MuxRuntimeCommandReceiver>,
    runtime_event_rx: Option<std::sync::mpsc::Receiver<brehon_types::RuntimeEvent>>,
    runtime_command_router: Option<Arc<dyn RuntimeCommandRouter>>,
    runtime_agent_factory_host_owned: bool,
    runtime_terminal_host_absolute_resize: bool,
    project_config_loader: research::ProjectConfigLoader,
) -> io::Result<()> {
    let mut terminal_guard = TerminalSessionGuard::new();
    let mut stdout = io::stdout();
    enter_dashboard_terminal_session(&mut stdout)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let started_at = Instant::now();
    let tick_active = Duration::from_millis(50); // ~20fps during visible output
    let tick_idle = Duration::from_millis(250); // ~4fps when idle/background-only output
    let idle_threshold = Duration::from_millis(500);
    let last_output_at = Instant::now();
    let group_tab = GroupTab::Dashboard;
    let click_regions: Vec<ClickRegion> = Vec::new();
    let selection: Option<SelectionState> = None;
    let pending_down: Option<(SelectionPane, String, PanePos)> = None;
    let pending_escape_sequence: Option<PendingEscapeSequence> = None;
    let left_pane_area = Rect::default();
    let supervisor_pane_area = Rect::default();
    let expanded_epics: std::collections::HashSet<String> = std::collections::HashSet::new();
    let expanded_activity_rows: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let input_mode = InputMode::default();
    let task_detail: Option<TaskDetailState> = None;
    let dashboard_agent_list = DashboardAgentListState::default();
    let dashboard_task_list = DashboardTaskListState::default();
    let structured_mode: std::collections::HashSet<String> = mux
        .panes()
        .filter(|p| p.is_gateway_backed())
        .map(|p| p.id().to_string())
        .collect();

    // Stall detection: track last PTY output per pane
    let mut last_activity: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    let auto_recover_threshold = Duration::from_secs(15 * 60);
    let review_obligation_nudge_threshold = Duration::from_secs(10 * 60);
    let review_obligation_reset_threshold = Duration::from_secs(30 * 60);
    let worker_context_reset_cooldown = Duration::from_secs(60);
    let self_improve_idle_threshold = Duration::from_secs(2 * 60);
    let self_improve_retry_cooldown = Duration::from_secs(10 * 60);
    let last_stall_check = std::time::Instant::now();
    let stall_check_interval = Duration::from_secs(30);
    let supervisor_dispatch_nudge_quiet_threshold = Duration::from_secs(20);
    let supervisor_dispatch_nudge_cooldown = Duration::from_secs(5 * 60);
    let last_supervisor_dispatch_nudge: Option<(String, std::time::Instant)> = None;
    let last_supervisor_reset: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    let last_worker_context_reset: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    let pending_self_improve_prompt: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    let next_self_improve_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let prompt_blocked_recovery_failed_panes: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Post-checkpoint handoff nudge tuning. 90 s of idle after a checkpoint
    // is long enough to rule out "worker is writing a follow-up message"
    // but short enough to rescue the task well before the 15-minute
    // authoritative-recycle fires. 10-minute cooldown prevents a retry
    // storm if the first nudge doesn't land.
    let post_checkpoint_nudge_threshold = Duration::from_secs(90);
    let post_checkpoint_nudge_cooldown = Duration::from_secs(10 * 60);
    let post_checkpoint_nudges_sent: std::collections::HashMap<
        (String, String, String),
        std::time::Instant,
    > = std::collections::HashMap::new();
    let review_obligation_notifications_sent: std::collections::HashMap<
        (String, String, String),
        std::time::Instant,
    > = std::collections::HashMap::new();
    let review_obligation_resends_sent: std::collections::HashMap<
        (String, String, String),
        std::time::Instant,
    > = std::collections::HashMap::new();
    let review_obligation_failures_reported: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    let active_worker_recovery_nudges_sent: std::collections::HashMap<
        (String, String),
        std::time::Instant,
    > = std::collections::HashMap::new();
    let active_worker_recovery_resets_sent: std::collections::HashMap<
        (String, String),
        std::time::Instant,
    > = std::collections::HashMap::new();

    // Collect pane IDs by kind
    let worker_ids: Vec<String> = mux
        .panes()
        .filter(|p| *p.kind() == PaneKind::Worker)
        .map(|p| p.id().to_string())
        .collect();
    let all_reviewer_ids: Vec<String> = mux
        .panes()
        .filter(|p| *p.kind() == PaneKind::Reviewer)
        .map(|p| p.id().to_string())
        .collect();
    let advisor_ids: Vec<String> = mux
        .panes()
        .filter(|p| *p.kind() == PaneKind::Advisor)
        .map(|p| p.id().to_string())
        .collect();
    let research_ids: Vec<String> = mux
        .panes()
        .filter(|p| *p.kind() == PaneKind::Research)
        .map(|p| p.id().to_string())
        .collect();
    let supervisor_id: Option<String> = mux
        .panes()
        .find(|p| *p.kind() == PaneKind::Supervisor)
        .map(|p| p.id().to_string());

    // Build reviewer panel structure
    let fallback_panels: Vec<ReviewerPanel> = if reviewer_panels.is_empty() {
        // Auto-group: each reviewer is its own panel (flat)
        all_reviewer_ids
            .iter()
            .map(|id| ReviewerPanel {
                name: id.clone(),
                members: vec![id.clone()],
            })
            .collect()
    } else {
        reviewer_panels.to_vec()
    };
    let has_panels = !fallback_panels.is_empty();
    let panels = fallback_panels.clone();

    // Selection state
    let selected_worker: usize = 0;
    let selected_panel: usize = 0;
    let selected_member: Vec<usize> = vec![0; panels.len()]; // per-panel member index
    let mut reviewer_selection = ReviewerSelectionState::default();
    capture_reviewer_selection_state(
        &panels,
        selected_panel,
        &selected_member,
        &mut reviewer_selection,
    );

    let pending_initial_resize = true;

    let last_session_poll = std::time::Instant::now();
    let session_poll_interval = Duration::from_secs(5);
    let runtime_session_name = mux.session_name().map(str::to_string);
    let last_shared_root_issue: Option<String> = None;
    let pending_dashboard_refresh: Option<tokio::task::JoinHandle<DashboardRefreshSnapshot>> = None;
    let pending_queued_gateway_prompt_deliveries: Vec<AsyncQueuedGatewayPromptDeliveryTask> =
        Vec::new();
    let pending_runtime_commands = Vec::new();
    let recent_runtime_commands = Vec::new();
    let pending_runtime_approval_resolutions = Vec::new();
    let prev_group_tab = group_tab;
    let initial_now = std::time::Instant::now();
    for pane in mux.panes() {
        last_activity.insert(pane.id().to_string(), initial_now);
    }

    let mut ctx = event_loop::EventLoopCtx {
        shutdown: shutdown.clone(),
        mux,
        runtime_command_rx,
        runtime_event_rx,
        runtime_command_router,
        runtime_agent_factory_host_owned,
        runtime_terminal_host_absolute_resize,
        rt: rt.clone(),
        terminal,
        dashboard_data,
        orchestration,
        tick_active,
        tick_idle,
        idle_threshold,
        last_output_at,
        started_at,
        group_tab,
        prev_group_tab,
        click_regions,
        selection,
        pending_down,
        pending_escape_sequence,
        left_pane_area,
        supervisor_pane_area,
        expanded_epics,
        expanded_activity_rows,
        structured_scroll_offsets: std::collections::HashMap::new(),
        input_mode,
        task_detail,
        advisor_room_view: AdvisorRoomViewState::default(),
        research_room_view: ResearchRoomViewState::default(),
        dashboard_agent_list,
        dashboard_task_list,
        structured_mode,
        last_activity,
        auto_recover_threshold,
        review_obligation_nudge_threshold,
        review_obligation_reset_threshold,
        worker_context_reset_cooldown,
        self_improve_idle_threshold,
        self_improve_retry_cooldown,
        last_stall_check,
        stall_check_interval,
        supervisor_dispatch_nudge_quiet_threshold,
        supervisor_dispatch_nudge_cooldown,
        last_supervisor_dispatch_nudge,
        last_supervisor_reset,
        last_worker_context_reset,
        pending_self_improve_prompt,
        next_self_improve_index,
        prompt_blocked_recovery_failed_panes,
        post_checkpoint_nudge_threshold,
        post_checkpoint_nudge_cooldown,
        post_checkpoint_nudges_sent,
        review_obligation_notifications_sent,
        review_obligation_resends_sent,
        review_obligation_failures_reported,
        active_worker_recovery_nudges_sent,
        active_worker_recovery_resets_sent,
        worker_ids,
        all_reviewer_ids,
        advisor_ids,
        research_ids,
        supervisor_id,
        fallback_panels,
        has_panels,
        panels,
        selected_worker,
        selected_panel,
        selected_member,
        reviewer_selection,
        pending_initial_resize,
        last_session_poll,
        session_poll_interval,
        runtime_session_name,
        last_shared_root_issue,
        pending_dashboard_refresh,
        pending_queued_gateway_prompt_deliveries,
        pending_runtime_commands,
        recent_runtime_commands,
        pending_runtime_approval_resolutions,
        entry_chrome_fade_complete: false,
        last_panesmith_snapshot_panes: std::collections::BTreeSet::new(),
        force_panesmith_snapshot_refresh: true,
        project_config_loader,
        needs_redraw: true,
    };

    event_loop::run(&mut ctx)?;

    rt.block_on(ctx.mux.shutdown_all());
    shutdown.store(true, Ordering::SeqCst);
    restore_terminal_session();
    terminal_guard.disarm();
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) fn init_test_git_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let status = std::process::Command::new("git")
        .arg("init")
        .arg(path)
        .status()
        .expect("git init");
    assert!(status.success(), "git init should succeed");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "user.name", "Brehon Test"])
        .status()
        .expect("git config user.name");
    assert!(status.success(), "git config user.name should succeed");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "user.email", "brehon@example.com"])
        .status()
        .expect("git config user.email");
    assert!(status.success(), "git config user.email should succeed");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "commit.gpgsign", "false"])
        .status()
        .expect("git config commit.gpgsign");
    assert!(status.success(), "git config commit.gpgsign should succeed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::StatusKind;
    use brehon_types::task::TaskStatus;
    use crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    // This test owns a real Panesmith PTY child and drives process-global mux
    // polling state, so keep it serial inside this test binary.
    #[cfg(unix)]
    static SERIAL_PANESMITH_STYLE_TEST: Mutex<()> = Mutex::new(());

    fn test_unix_timestamp_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }

    fn write_test_task(root: &Path, task_id: &str, status: &str) {
        let tasks_dir = root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task = serde_json::json!({
            "task_id": task_id,
            "status": status,
            "title": format!("Task {task_id}")
        });
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
    }

    fn make_task(
        id: &str,
        title: &str,
        status: &str,
        task_type: &str,
        parent_id: Option<&str>,
    ) -> TaskInfo {
        TaskInfo {
            id: id.to_string(),
            title: title.to_string(),
            status: status.to_string(),
            assignee: Some("worker-1".to_string()),
            task_type: task_type.to_string(),
            parent_id: parent_id.map(ToOwned::to_owned),
            description: format!("Detailed brief for {id}"),
            priority: Some("high".to_string()),
            percent: Some(50),
            tokens_used: 0,
            completion_mode: Some("merge".to_string()),
            merge_target: Some("main".to_string()),
            integration_status: None,
            integration_branch: None,
            integration_worktree: None,
            activity: Some("editing".to_string()),
            notes: Some("Latest progress note".to_string()),
            blockers: None,
            dependencies: Vec::new(),
            blocked_by: Vec::new(),
            created_at: Some("2026-04-06T00:00:00Z".to_string()),
            updated_at: Some("2026-04-06T00:01:00Z".to_string()),
            closed_at: None,
            closed_by: None,
            merged_commit: None,
            merged_branch: None,
            latest_commit: None,
            run: None,
            review_id: None,
            review_status: None,
            review_round: None,
            review_panel_id: None,
            review_panel_members: Vec::new(),
            review_panel_lease_state: None,
            review_feedback_outcome: None,
            review_feedback_threshold_reason: None,
            review_feedback_evaluated_at: None,
            review_feedback_blocking: Vec::new(),
            review_feedback_suggestions: Vec::new(),
            review_feedback_nitpicks: Vec::new(),
            review_feedback_dissent: Vec::new(),
            integration_conflict_owner: None,
            integration_conflict_source: None,
            integration_conflict_merge_target: None,
            integration_conflict_reviewed_commit: None,
            integration_conflict_previous_worker: None,
            integration_conflict_conflicting_files: Vec::new(),
            acceptance_criteria: vec!["Task works from the dashboard".to_string()],
            file_hints: vec!["crates/brehon-tui/src/run.rs".to_string()],
            constraints: vec![],
            test_requirements: vec!["cargo test -p brehon-tui".to_string()],
            plan_steps: vec!["Render dialog".to_string()],
            implementation_notes: Some("Keep click targets separate.".to_string()),
            research_context: Vec::new(),
            proof: None,
            feedback: None,
        }
    }
    fn lines_to_string(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_find_review_wait_task_for_worker_only_returns_review_held_task() {
        let mut pending = make_task("T-pending", "Pending", "pending", "task", None);
        pending.assignee = Some("worker-1".to_string());
        pending.updated_at = Some("2026-04-06T00:01:00Z".to_string());

        let mut review_ready = make_task("T-review", "Review", "review_ready", "task", None);
        review_ready.assignee = Some("worker-1".to_string());
        review_ready.updated_at = Some("2026-04-06T00:02:00Z".to_string());

        let tasks = [pending, review_ready];
        let selected = find_review_wait_task_for_worker(&tasks, "worker-1").expect("task");
        assert_eq!(selected.id, "T-review");
    }

    #[test]
    fn test_next_self_improvement_prompt_skips_mutating_tasks_when_disallowed() {
        let task = make_task("T-scope", "Scoped task", "review_ready", "task", None);
        let tasks = vec!["fix_warnings".to_string(), "run_tests".to_string()];

        let (index, task_name, prompt) =
            next_self_improvement_prompt(&task, &tasks, false, 0).expect("prompt");

        assert_eq!(index, 1);
        assert_eq!(task_name, "run_tests");
        assert!(prompt.contains("Do not edit files"));
        assert!(prompt.contains("T-scope"));
    }

    #[test]
    fn test_build_task_scoped_self_improvement_prompt_is_limited_to_current_task() {
        let task = make_task(
            "T-reviewscope",
            "Review scoped task",
            "in_review",
            "task",
            None,
        );

        let prompt =
            build_task_scoped_self_improvement_prompt(&task, "collect_review_notes", false)
                .expect("prompt");

        assert!(prompt.contains("T-reviewscope"));
        assert!(prompt.contains("Review scoped task"));
        assert!(prompt.contains("Do NOT call `task action=mine`"));
        assert!(prompt
            .contains("Do NOT inspect, checkout, or modify any other task, worktree, or branch"));
        assert!(prompt.contains("Do not edit files, create commits, or change task status"));
    }

    fn make_reviewer_pane(name: &str) -> brehon_mux::Pane {
        make_reviewer_pane_with_agent_type(name, brehon_mux::SupervisorCli::Codex, None)
    }

    fn make_reviewer_pane_with_agent_type(
        name: &str,
        adapter: brehon_mux::SupervisorCli,
        configured_agent_type: Option<&str>,
    ) -> brehon_mux::Pane {
        let dir = tempfile::tempdir().expect("tempdir");
        brehon_mux::Pane::reviewer_with_agent_type(
            name,
            dir.path().to_path_buf(),
            None,
            None,
            24,
            80,
            &brehon_mux::AgentAdapter::BuiltIn(adapter),
            None,
            None,
            None,
            configured_agent_type,
            None,
            &[],
            None,
        )
        .expect("create reviewer pane")
    }

    fn make_worker_pane_with_adapter(
        name: &str,
        configured_agent_type: &str,
        adapter: &brehon_mux::AgentAdapter,
    ) -> brehon_mux::Pane {
        let dir = tempfile::tempdir().expect("tempdir");
        brehon_mux::Pane::worker_with_agent_type(
            name,
            dir.path().to_path_buf(),
            None,
            None,
            "supervisor",
            adapter,
            None,
            None,
            24,
            80,
            None,
            None,
            Some(configured_agent_type),
            &[],
            None,
        )
        .expect("create worker pane")
    }

    fn make_custom_worker_pane(name: &str, provider: &str) -> brehon_mux::Pane {
        let adapter = brehon_mux::AgentAdapter::Custom(brehon_mux::CustomAgentConfig {
            name: provider.to_string(),
            command: Some("codex".to_string()),
            args: vec![
                "-c".to_string(),
                "model_provider=\"ollama_cloud\"".to_string(),
                "app-server".to_string(),
            ],
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            capabilities: brehon_mux::HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: brehon_mux::PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
                transport: brehon_mux::HarnessTransport::AppServer,
                preferred_control_plane: brehon_mux::HarnessControlPlane::Acp,
            },
        });

        make_worker_pane_with_adapter(name, provider, &adapter)
    }

    fn custom_interactive_agent(
        name: &str,
        command: &str,
        args: &[&str],
    ) -> brehon_mux::AgentAdapter {
        brehon_mux::AgentAdapter::Custom(brehon_mux::CustomAgentConfig {
            name: name.to_string(),
            command: Some(command.to_string()),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            capabilities: brehon_mux::HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: brehon_mux::PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: brehon_mux::HarnessTransport::InteractivePty,
                preferred_control_plane: brehon_mux::HarnessControlPlane::PtyInjection,
            },
        })
    }

    fn make_worker_pane(name: &str) -> brehon_mux::Pane {
        let dir = tempfile::tempdir().expect("tempdir");
        brehon_mux::Pane::worker(
            name,
            dir.path().to_path_buf(),
            None,
            "codex-worker",
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("create worker pane")
    }

    fn make_builtin_worker_pane(
        name: &str,
        configured_agent_type: &str,
        cli: brehon_mux::SupervisorCli,
    ) -> brehon_mux::Pane {
        let adapter = brehon_mux::AgentAdapter::Custom(brehon_mux::CustomAgentConfig {
            name: cli.as_str().to_string(),
            command: Some(cli.as_str().to_string()),
            args: vec![],
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            capabilities: cli.capabilities(),
        });

        make_worker_pane_with_adapter(name, configured_agent_type, &adapter)
    }

    fn headless_host_owned_dashboard_status(pane_id: &str) -> RuntimeDaemonDashboardStatus {
        RuntimeDaemonDashboardStatus {
            generated_at_ms: 1,
            running: true,
            metrics: RuntimeDaemonDashboardMetrics::default(),
            registry_count: 1,
            registry: RuntimePaneRegistryDashboardSnapshot {
                panes: vec![RuntimePaneDashboardInfo {
                    session_id: "session-1".to_string(),
                    pane_id: pane_id.to_string(),
                    generation: 2,
                    state: brehon_types::RuntimePaneState::Ready,
                    kind: brehon_types::RuntimePaneKind::Worker,
                    source: Some(brehon_types::RuntimeSource::Headless),
                    title: Some(pane_id.to_string()),
                    last_output_ms: Some(1234),
                    exit_code: None,
                    exit_reason: None,
                    blocked: None,
                }],
            },
            approvals: RuntimeApprovalDashboardSnapshot::default(),
            sidecar: None,
            terminal_host: Some(RuntimeTerminalHostDashboardStatus {
                kind: brehon_types::RuntimeTerminalHostKind::Headless,
                experimental: true,
                observation_running: true,
                command_routing: RuntimeTerminalHostCommandRoutingDashboard::TerminalHost,
                pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Host,
                agent_factory: RuntimeTerminalHostAgentFactoryRoutingDashboard::TerminalHost,
                capabilities: None,
                promotion_readiness: RuntimeTerminalHostPromotionReadinessDashboard::default(),
                session_name: Some("brehon-session".to_string()),
                socket_name: None,
                socket_dir: None,
                binary_path: None,
                diagnostics: Vec::new(),
            }),
        }
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
        .expect("create supervisor pane")
    }

    fn buffer_row_string(buffer: &ratatui::buffer::Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, row)).map(|cell| cell.symbol()))
            .collect::<Vec<_>>()
            .join("")
            .trim_end()
            .to_string()
    }

    fn buffer_text_cell(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
        for y in 0..buffer.area.height {
            let row = (0..buffer.area.width)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                .collect::<Vec<_>>()
                .join("");
            if let Some(x) = row.find(needle) {
                return Some((x as u16, y));
            }
        }
        None
    }

    fn panesmith_row_text(row: &panesmith::SurfaceRow<'_>) -> String {
        row.cells
            .iter()
            .map(|cell| cell.text.as_ref())
            .collect::<String>()
    }

    fn panesmith_style_for_text_in_row(
        row: &panesmith::SurfaceRow<'_>,
        needle: &str,
    ) -> Option<panesmith::CellStyle> {
        let row_text = panesmith_row_text(row);
        let target_start = row_text.find(needle)?;
        let mut byte_offset = 0;
        for cell in &row.cells {
            let text = cell.text.as_ref();
            let next_offset = byte_offset + text.len();
            if next_offset > target_start && !text.trim().is_empty() {
                return Some(cell.style);
            }
            byte_offset = next_offset;
        }
        None
    }

    fn panesmith_snapshot_style_for_text(
        snapshot: &panesmith::OwnedPaneSnapshot,
        needle: &str,
    ) -> Option<panesmith::CellStyle> {
        snapshot
            .surface
            .rows
            .iter()
            .find_map(|row| panesmith_style_for_text_in_row(row, needle))
    }

    fn panesmith_scrollback_style_for_text(
        scrollback: &panesmith::OwnedScrollbackSnapshot,
        needle: &str,
    ) -> Option<panesmith::CellStyle> {
        scrollback
            .lines
            .iter()
            .find_map(|line| panesmith_style_for_text_in_row(&line.row, needle))
    }

    fn color_label(color: Color) -> String {
        match color {
            Color::Reset => "reset".to_string(),
            Color::White => "white".to_string(),
            Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
            other => format!("{other:?}"),
        }
    }

    fn write_test_review_state(root: &Path, task_id: &str, review_id: &str, status: &str) {
        let review_dir = root.join("runtime").join("reviews").join(task_id);
        std::fs::create_dir_all(&review_dir).unwrap();
        let state = serde_json::json!({
            "task_id": task_id,
            "status": status,
            "current_round": 1,
            "current_review_id": review_id,
            "max_rounds": 3,
            "panel": ["reviewer-1"],
            "submissions_received": [],
            "created_at": "2026-04-02T00:00:00Z",
            "updated_at": "2026-04-02T00:00:00Z"
        });
        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_read_pending_review_obligations_only_targets_missing_reviewers() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-review", "in_review");
        let review_dir = temp.path().join("runtime").join("reviews").join("T-review");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-review",
                "status": "collecting",
                "current_round": 2,
                "current_review_id": "REV-live",
                "panel_id": "primary",
                "panel": ["reviewer-a", "reviewer-b", "reviewer-c"],
                "submissions_received": ["reviewer-a"],
                "created_at": "2026-04-02T00:00:00Z",
                "updated_at": "2026-04-02T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        let obligations = read_pending_review_obligations(temp.path(), &tasks);
        assert!(!obligations.contains_key("reviewer-a"));
        assert_eq!(obligations["reviewer-b"].len(), 1);
        assert_eq!(obligations["reviewer-c"].len(), 1);
        assert_eq!(obligations["reviewer-b"][0].task_id, "T-review");
        assert_eq!(obligations["reviewer-b"][0].review_id, "REV-live");
        assert_eq!(
            obligations["reviewer-b"][0].panel_id.as_deref(),
            Some("primary")
        );
        assert_eq!(obligations["reviewer-b"][0].round, Some(2));
        assert_eq!(obligations["reviewer-b"][0].pending_reviewers, 2);
    }

    #[test]
    fn test_read_pending_review_obligations_ignores_stale_collecting_state_for_non_review_task() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-stale", "changes_requested");
        let review_dir = temp.path().join("runtime").join("reviews").join("T-stale");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-stale",
                "status": "collecting",
                "current_round": 1,
                "current_review_id": "REV-stale",
                "panel_id": "primary",
                "panel": ["reviewer-a", "reviewer-b"],
                "submissions_received": [],
                "created_at": "2026-04-02T00:00:00Z",
                "updated_at": "2026-04-02T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        let obligations = read_pending_review_obligations(temp.path(), &tasks);
        assert!(
            obligations.is_empty(),
            "non-review tasks should not produce live reviewer obligations from stale collecting state"
        );
    }

    #[test]
    fn test_read_pending_review_obligations_populates_assignment_delivery_fields() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-review", "in_review");
        let review_dir = temp.path().join("runtime").join("reviews").join("T-review");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-review",
                "status": "collecting",
                "current_round": 1,
                "current_review_id": "REV-live",
                "panel_id": "primary",
                "panel": ["reviewer-a"],
                "submissions_received": [],
                "reviewer_assignments": {
                    "reviewer-a": {
                        "owner": "reviewer-a",
                        "assignment_kind": "review",
                        "assigned_at": "2026-04-02T00:00:00Z",
                        "prompt_id": "prompt+reviewer$a",
                        "delivery_method": "queued",
                        "acknowledged_at": "2026-04-02T00:01:00Z"
                    }
                },
                "created_at": "2026-04-02T00:00:00Z",
                "updated_at": "2026-04-02T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        let prompt_id = "prompt+reviewer$a";
        let enqueue_dir = temp.path().join("runtime").join("prompt-enqueue-acks");
        std::fs::create_dir_all(&enqueue_dir).unwrap();
        std::fs::write(
            enqueue_dir.join(format!(
                "{}.json",
                crate::run::helpers::sanitize_runtime_key(prompt_id)
            )),
            serde_json::json!({
                "prompt_id": prompt_id
            })
            .to_string(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        let obligations = read_pending_review_obligations(temp.path(), &tasks);
        let obligation = &obligations["reviewer-a"][0];
        assert_eq!(
            obligation.assignment_delivery_state.as_deref(),
            Some("drained_without_ack")
        );
        assert_eq!(
            obligation.assignment_acknowledged_at.as_deref(),
            Some("2026-04-02T00:01:00Z")
        );
    }

    #[test]
    fn test_sync_reviewer_review_contexts_marks_pending_panel_members() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-review", "in_review");
        let review_dir = temp.path().join("runtime").join("reviews").join("T-review");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-review",
                "status": "collecting",
                "current_round": 3,
                "current_review_id": "REV-live",
                "panel_id": "primary",
                "panel": ["reviewer-a", "reviewer-b", "reviewer-c"],
                "submissions_received": ["reviewer-a"],
                "created_at": "2026-05-23T00:00:00Z",
                "updated_at": "2026-05-23T00:01:00Z"
            }))
            .unwrap(),
        )
        .unwrap();

        let mut mux = brehon_mux::Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane("reviewer-a"));
        mux.add_pane(make_reviewer_pane("reviewer-b"));
        mux.add_pane(make_reviewer_pane("reviewer-c"));
        mux.add_pane(make_reviewer_pane("reviewer-idle"));

        let tasks = read_task_files(temp.path());
        sync_reviewer_review_contexts(&mut mux, temp.path(), &tasks);

        assert!(mux
            .get("reviewer-a")
            .expect("reviewer-a")
            .review_context()
            .is_none());
        let reviewer_b = mux
            .get("reviewer-b")
            .expect("reviewer-b")
            .review_context()
            .expect("pending reviewer context");
        assert_eq!(reviewer_b.review_id, "REV-live");
        assert_eq!(reviewer_b.task_id, "T-review");
        assert_eq!(reviewer_b.round, 3);
        assert_eq!(reviewer_b.panel_done, 1);
        assert_eq!(reviewer_b.panel_total, 3);
        assert!(mux
            .get("reviewer-c")
            .expect("reviewer-c")
            .review_context()
            .is_some());
        assert!(mux
            .get("reviewer-idle")
            .expect("reviewer-idle")
            .review_context()
            .is_none());
    }

    #[test]
    fn test_collect_dashboard_refresh_collects_runtime_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        init_test_git_repo(temp.path());

        let brehon_root = temp.path().join(".brehon");
        write_test_task(&brehon_root, "T-refresh", "in_progress");

        let leases_dir = brehon_root.join("runtime").join("review-panels");
        std::fs::create_dir_all(&leases_dir).unwrap();
        std::fs::write(
            leases_dir.join("primary.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "panel_id": "primary",
                "task_id": "T-refresh",
                "members": ["reviewer-1"],
            }))
            .unwrap(),
        )
        .unwrap();

        let snapshot = collect_dashboard_refresh(
            &brehon_root,
            &[SessionRefreshEntry {
                name: "worker-1".to_string(),
                role: "worker".to_string(),
                session_id: "sess-1".to_string(),
                agent_type: "codex-worker-5-4".to_string(),
            }],
            &[ReviewerPanel {
                name: "primary".to_string(),
                members: vec!["reviewer-1".to_string()],
            }],
        );

        assert!(snapshot.tasks.iter().any(|task| task.id == "T-refresh"));
        let session = snapshot.sessions.get("worker-1").expect("worker session");
        assert_eq!(session.0, "worker");
        assert_eq!(session.1, "sess-1");
        assert!(!session.2.is_empty());
        assert_eq!(
            snapshot
                .panels
                .iter()
                .find(|panel| panel.name == "primary")
                .expect("primary panel")
                .members,
            vec!["reviewer-1".to_string()]
        );
        assert_eq!(snapshot.shared_root_issue, None);
    }

    #[test]
    fn test_collect_dashboard_refresh_detects_untracked_root_leak() {
        let temp = tempfile::tempdir().unwrap();
        init_test_git_repo(temp.path());

        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();
        std::fs::write(temp.path().join("leaked-root-file.txt"), "oops").unwrap();

        let snapshot = collect_dashboard_refresh(&brehon_root, &[], &[]);
        let issue = snapshot
            .shared_root_issue
            .expect("untracked shared-root file should be reported");
        assert!(issue.contains("leaked-root-file.txt"));
    }

    #[test]
    fn test_collect_session_refresh_entries_skips_dead_and_exited_panes() {
        let mut mux = Mux::new(24, 80);

        let live = make_worker_pane("worker-live");
        assert!(live.agent_session_id().is_some());
        mux.add_pane(live);

        let mut exited = make_worker_pane("worker-exited");
        assert!(exited.agent_session_id().is_some());
        exited.mark_exited(Some(1));
        mux.add_pane(exited);

        let dead = make_worker_pane("worker-dead");
        assert!(dead.agent_session_id().is_some());
        mux.add_pane(dead);
        mux.quarantine("worker-dead", brehon_mux::DeathReason::SessionDropped);

        let entries = collect_session_refresh_entries(&mux);
        let mut names = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(names, vec!["worker-live"]);
    }

    #[test]
    fn test_read_runtime_daemon_dashboard_status_reads_pending_approvals() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        let command = brehon_types::RuntimeCommand {
            command_id: "cmd-1".to_string(),
            target: brehon_types::RuntimeCommandTarget {
                session_id: "session-1".to_string(),
                pane_id: Some("worker-1".to_string()),
                generation: Some(1),
            },
            issued_at_ms: 1,
            kind: brehon_types::RuntimeCommandKind::Interrupt {
                reason: "operator".to_string(),
            },
        };
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": 1,
                "running": true,
                "metrics": {
                    "published_events": 2,
                    "routed_commands": 3,
                    "rejected_commands": 0,
                    "deferred_commands": 1,
                    "pending_approvals": 1,
                    "audit_write_errors": 0
                },
                "registry_count": 4,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": [{
                        "session_id": "session-1",
                        "pane_id": "worker-1",
                        "generation": 1,
                        "state": "ready",
                        "kind": "worker",
                        "source": "headless",
                        "title": "Worker 1",
                        "last_event_ms": 1,
                        "last_output_ms": 2
                    }]
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": [{
                        "approval_id": "approval-1",
                        "reason": "operation requires explicit approval",
                        "command": command
                    }]
                },
                "sidecar": {
                    "detection_running": true,
                    "workflow_running": true
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let status = read_runtime_daemon_dashboard_status(&brehon_root).expect("status");
        assert!(status.running);
        assert_eq!(status.registry_count, 4);
        assert_eq!(status.registry.panes.len(), 1);
        assert_eq!(status.registry.panes[0].pane_id, "worker-1");
        assert_eq!(
            status.registry.panes[0].state,
            brehon_types::RuntimePaneState::Ready
        );
        assert_eq!(
            status.terminal_host.expect("terminal host").kind,
            brehon_types::RuntimeTerminalHostKind::Headless
        );
        assert_eq!(status.approvals.approvals.len(), 1);
        assert_eq!(status.approvals.approvals[0].approval_id, "approval-1");
    }

    #[test]
    fn test_attempt_auto_recover_stalled_worker_reassigns_clean_task() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();
        let worktree = brehon_root
            .join("worktrees")
            .join("runs")
            .join("run-1")
            .join("worker-1");
        init_test_git_repo(&worktree);

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-stalled.json"),
            serde_json::json!({
                "task_id": "T-stalled",
                "title": "Stalled task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1",
                "review_owner": "worker-1"
            })
            .to_string(),
        )
        .unwrap();

        let mut stalled = make_task("T-stalled", "Stalled task", "in_progress", "task", None);
        stalled.assignee = Some("worker-1".to_string());
        let tasks = vec![stalled];
        let sessions = std::collections::HashMap::from([
            (
                "worker-1".to_string(),
                (
                    "worker".to_string(),
                    "sess-1".to_string(),
                    "now".to_string(),
                ),
            ),
            (
                "worker-2".to_string(),
                (
                    "worker".to_string(),
                    "sess-2".to_string(),
                    "now".to_string(),
                ),
            ),
        ]);

        let outcome =
            attempt_auto_recover_stalled_worker(brehon_root, "worker-1", &tasks, &sessions, 20);
        assert_eq!(
            outcome,
            Some(StalledRecoveryOutcome::Reassigned {
                task_id: "T-stalled".to_string(),
                old_worker: "worker-1".to_string(),
                new_worker: "worker-2".to_string(),
            })
        );

        let saved = read_raw_task_file(brehon_root, "T-stalled").unwrap();
        assert_eq!(saved["status"], "assigned");
        assert_eq!(saved["assignee"], "worker-2");
        assert_eq!(saved["review_owner"], serde_json::Value::Null);
        assert!(saved["recovery_note"]
            .as_str()
            .unwrap_or("")
            .contains("Automatically recovered stalled task"));
    }

    #[test]
    fn test_attempt_auto_recover_stalled_worker_normalizes_completed_task_to_review_ready() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();
        let worktree = brehon_root
            .join("worktrees")
            .join("runs")
            .join("run-1")
            .join("worker-1");
        init_test_git_repo(&worktree);

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-stalled.json"),
            serde_json::json!({
                "task_id": "T-stalled",
                "title": "Stalled task",
                "status": "assigned",
                "task_type": "task",
                "completion_mode": "close",
                "assignee": "worker-1",
                "percent": 100
            })
            .to_string(),
        )
        .unwrap();

        let mut stalled = make_task("T-stalled", "Stalled task", "assigned", "task", None);
        stalled.assignee = Some("worker-1".to_string());
        stalled.percent = Some(100);
        let tasks = vec![stalled];
        let sessions = std::collections::HashMap::from([(
            "worker-1".to_string(),
            (
                "worker".to_string(),
                "sess-1".to_string(),
                "now".to_string(),
            ),
        )]);

        let outcome =
            attempt_auto_recover_stalled_worker(brehon_root, "worker-1", &tasks, &sessions, 20);
        assert_eq!(
            outcome,
            Some(StalledRecoveryOutcome::ReviewReady {
                task_id: "T-stalled".to_string(),
                worker: "worker-1".to_string(),
            })
        );

        let saved = read_raw_task_file(brehon_root, "T-stalled").unwrap();
        assert_eq!(saved["status"], "review_ready");
        assert_eq!(saved["assignee"], "worker-1");
        assert_eq!(saved["review_owner"], "worker-1");
        assert!(saved["recovery_note"]
            .as_str()
            .unwrap_or("")
            .contains("moved to review_ready"));
    }

    #[test]
    fn test_promote_active_assigned_task_marks_task_in_progress() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-active.json"),
            serde_json::json!({
                "task_id": "T-active",
                "title": "Active task",
                "status": "assigned",
                "task_type": "task",
                "assignee": "worker-1",
                "percent": 15
            })
            .to_string(),
        )
        .unwrap();

        let outcome = promote_active_assigned_task(brehon_root, "T-active", "worker-1");
        assert_eq!(outcome, Some("in_progress"));

        let saved = read_raw_task_file(brehon_root, "T-active").unwrap();
        assert_eq!(saved["status"], "in_progress");
        assert_eq!(saved["assignee"], "worker-1");
    }

    #[test]
    fn test_promote_active_assigned_task_does_not_enter_review_ready_without_commit() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-active.json"),
            serde_json::json!({
                "task_id": "T-active",
                "title": "Implement active task",
                "description": "Make code changes in src/lib.rs",
                "status": "assigned",
                "task_type": "task",
                "assignee": "worker-1",
                "percent": 100
            })
            .to_string(),
        )
        .unwrap();

        let outcome = promote_active_assigned_task(brehon_root, "T-active", "worker-1");
        assert_eq!(outcome, Some("in_progress"));

        let saved = read_raw_task_file(brehon_root, "T-active").unwrap();
        assert_eq!(saved["status"], "in_progress");
        assert!(saved.get("review_owner").is_none());
    }

    #[test]
    fn test_promote_active_assigned_task_allows_close_mode_review_ready_without_commit() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-active.json"),
            serde_json::json!({
                "task_id": "T-active",
                "title": "Document baseline",
                "description": "Update documentation only",
                "status": "assigned",
                "task_type": "task",
                "completion_mode": "close",
                "assignee": "worker-1",
                "percent": 100
            })
            .to_string(),
        )
        .unwrap();

        let outcome = promote_active_assigned_task(brehon_root, "T-active", "worker-1");
        assert_eq!(outcome, Some("review_ready"));

        let saved = read_raw_task_file(brehon_root, "T-active").unwrap();
        assert_eq!(saved["status"], "review_ready");
        assert_eq!(saved["review_owner"], "worker-1");
    }

    #[test]
    fn test_quarantined_worker_names_only_returns_live_owned_workers() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();

        let health_dir = brehon_root.join("runtime").join("agent-health");
        std::fs::create_dir_all(&health_dir).unwrap();
        std::fs::write(
            agent_health_path(brehon_root, "worker-1"),
            serde_json::json!({
                "status": "unavailable",
                "error": "quota will reset",
            })
            .to_string(),
        )
        .unwrap();

        let mut active = make_task("T-owned", "Owned task", "assigned", "task", None);
        active.assignee = Some("worker-1".to_string());
        let mut other = make_task("T-other", "Other task", "assigned", "task", None);
        other.assignee = Some("worker-2".to_string());
        let tasks = vec![active, other];

        let sessions = std::collections::HashMap::from([
            (
                "worker-1".to_string(),
                (
                    "worker".to_string(),
                    "sess-1".to_string(),
                    "now".to_string(),
                ),
            ),
            (
                "worker-2".to_string(),
                (
                    "worker".to_string(),
                    "sess-2".to_string(),
                    "now".to_string(),
                ),
            ),
            (
                "reviewer-1".to_string(),
                (
                    "reviewer".to_string(),
                    "sess-r".to_string(),
                    "now".to_string(),
                ),
            ),
        ]);

        let quarantined = quarantined_worker_names(brehon_root, &tasks, &sessions);
        assert_eq!(quarantined, vec!["worker-1".to_string()]);
    }

    #[test]
    fn test_idle_worker_names_keeps_completed_review_bound_tasks_reserved() {
        let mut completed = make_task("T-done", "Completed", "assigned", "task", None);
        completed.assignee = Some("worker-1".to_string());
        completed.percent = Some(100);
        let tasks = vec![completed];
        let sessions = std::collections::HashMap::from([
            (
                "worker-1".to_string(),
                (
                    "worker".to_string(),
                    "sess-1".to_string(),
                    "now".to_string(),
                ),
            ),
            (
                "worker-2".to_string(),
                (
                    "worker".to_string(),
                    "sess-2".to_string(),
                    "now".to_string(),
                ),
            ),
        ]);

        let idle = idle_worker_names(&tasks, &sessions, "worker-2");
        assert!(idle.is_empty(), "{idle:?}");
    }

    #[test]
    fn test_attempt_auto_recover_stalled_worker_blocks_dirty_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();
        let worktree = brehon_root
            .join("worktrees")
            .join("runs")
            .join("run-1")
            .join("worker-1");
        init_test_git_repo(&worktree);
        std::fs::write(worktree.join("dirty.txt"), "pending changes\n").unwrap();

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-stalled.json"),
            serde_json::json!({
                "task_id": "T-stalled",
                "title": "Stalled task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1"
            })
            .to_string(),
        )
        .unwrap();

        let mut stalled = make_task("T-stalled", "Stalled task", "in_progress", "task", None);
        stalled.assignee = Some("worker-1".to_string());
        let tasks = vec![stalled];
        let sessions = std::collections::HashMap::from([(
            "worker-1".to_string(),
            (
                "worker".to_string(),
                "sess-1".to_string(),
                "now".to_string(),
            ),
        )]);

        let outcome =
            attempt_auto_recover_stalled_worker(brehon_root, "worker-1", &tasks, &sessions, 20);
        assert_eq!(
            outcome,
            Some(StalledRecoveryOutcome::Blocked {
                task_id: "T-stalled".to_string(),
                worker: "worker-1".to_string(),
                reason: "worktree has uncommitted changes".to_string(),
            })
        );

        let saved = read_raw_task_file(brehon_root, "T-stalled").unwrap();
        assert_eq!(saved["status"], "in_progress");
        assert_eq!(saved["assignee"], "worker-1");
    }

    #[test]
    fn test_attempt_auto_recover_stalled_worker_blocks_missing_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-stalled.json"),
            serde_json::json!({
                "task_id": "T-stalled",
                "title": "Stalled task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1"
            })
            .to_string(),
        )
        .unwrap();

        let mut stalled = make_task("T-stalled", "Stalled task", "in_progress", "task", None);
        stalled.assignee = Some("worker-1".to_string());
        let tasks = vec![stalled];
        let sessions = std::collections::HashMap::from([
            (
                "worker-1".to_string(),
                (
                    "worker".to_string(),
                    "sess-1".to_string(),
                    "now".to_string(),
                ),
            ),
            (
                "worker-2".to_string(),
                (
                    "worker".to_string(),
                    "sess-2".to_string(),
                    "now".to_string(),
                ),
            ),
        ]);

        let outcome =
            attempt_auto_recover_stalled_worker(brehon_root, "worker-1", &tasks, &sessions, 20);
        assert_eq!(
            outcome,
            Some(StalledRecoveryOutcome::ManualRecoveryRequired {
                task_id: "T-stalled".to_string(),
                worker: "worker-1".to_string(),
                reason: "worker worktree is missing; manual recovery is required".to_string(),
            })
        );

        let saved = read_raw_task_file(brehon_root, "T-stalled").unwrap();
        assert_eq!(saved["status"], "in_progress");
        assert_eq!(saved["assignee"], "worker-1");
        assert!(saved.get("recovery_note").is_none());
    }

    #[test]
    fn test_attempt_auto_recover_stalled_worker_escalates_unmerged_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path();
        let worktree = brehon_root
            .join("worktrees")
            .join("runs")
            .join("run-1")
            .join("worker-1");
        init_test_git_repo(&worktree);

        std::fs::write(worktree.join("shared.txt"), "base\n").unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["add", "shared.txt"])
            .status()
            .expect("git add");
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["commit", "-m", "base"])
            .status()
            .expect("git commit");
        assert!(status.success());

        let default_branch_output = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["branch", "--show-current"])
            .output()
            .expect("git branch --show-current");
        assert!(default_branch_output.status.success());
        let default_branch = String::from_utf8_lossy(&default_branch_output.stdout)
            .trim()
            .to_string();

        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["checkout", "-b", "other"])
            .status()
            .expect("git checkout other");
        assert!(status.success());
        std::fs::write(worktree.join("shared.txt"), "other\n").unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["add", "shared.txt"])
            .status()
            .expect("git add other");
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["commit", "-m", "other"])
            .status()
            .expect("git commit other");
        assert!(status.success());

        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["checkout", &default_branch])
            .status()
            .expect("git checkout default branch");
        assert!(status.success());
        std::fs::write(worktree.join("shared.txt"), "master\n").unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["add", "shared.txt"])
            .status()
            .expect("git add master");
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["commit", "-m", "master"])
            .status()
            .expect("git commit master");
        assert!(status.success());

        let status = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["merge", "other"])
            .status()
            .expect("git merge other");
        assert!(!status.success(), "merge should conflict");

        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-stalled.json"),
            serde_json::json!({
                "task_id": "T-stalled",
                "title": "Stalled task",
                "status": "in_progress",
                "task_type": "task",
                "assignee": "worker-1",
                "review_owner": "worker-1",
                "merge_target": "epic/test",
                "latest_commit": "abc123"
            })
            .to_string(),
        )
        .unwrap();

        let mut stalled = make_task("T-stalled", "Stalled task", "in_progress", "task", None);
        stalled.assignee = Some("worker-1".to_string());
        stalled.merge_target = Some("epic/test".to_string());
        let tasks = vec![stalled];
        let sessions = std::collections::HashMap::from([(
            "worker-1".to_string(),
            (
                "worker".to_string(),
                "sess-1".to_string(),
                "now".to_string(),
            ),
        )]);

        let outcome =
            attempt_auto_recover_stalled_worker(brehon_root, "worker-1", &tasks, &sessions, 20);
        assert_eq!(
            outcome,
            Some(StalledRecoveryOutcome::SupervisorConflict {
                task_id: "T-stalled".to_string(),
                worker: "worker-1".to_string(),
                files: vec!["shared.txt".to_string()],
            })
        );

        let saved = read_raw_task_file(brehon_root, "T-stalled").unwrap();
        assert_eq!(saved["status"], "changes_requested");
        assert_eq!(saved["assignee"], serde_json::Value::Null);
        assert_eq!(saved["review_owner"], serde_json::Value::Null);
        assert_eq!(saved["activity"], "integration_conflict");
        assert_eq!(saved["integration_conflict"]["owner"], "supervisor");
        assert_eq!(saved["integration_conflict"]["source"], "worker_unmerged");
        assert_eq!(
            saved["integration_conflict"]["conflicting_files"][0],
            "shared.txt"
        );
    }

    #[test]
    fn test_parse_raw_sgr_mouse_sequence_scroll_down() {
        match parse_raw_sgr_mouse_sequence(b"\x1b[<65;188;25M") {
            RawSgrMouseParse::Complete(mouse) => {
                assert_eq!(mouse.kind, MouseEventKind::ScrollDown);
                assert_eq!(mouse.column, 187);
                assert_eq!(mouse.row, 24);
                assert_eq!(mouse.modifiers, KeyModifiers::empty());
            }
            _ => panic!("expected parsed mouse event"),
        }
    }

    #[test]
    fn test_parse_raw_sgr_mouse_sequence_left_release() {
        match parse_raw_sgr_mouse_sequence(b"\x1b[<0;20;10m") {
            RawSgrMouseParse::Complete(mouse) => {
                assert_eq!(mouse.kind, MouseEventKind::Up(MouseButton::Left));
                assert_eq!(mouse.column, 19);
                assert_eq!(mouse.row, 9);
            }
            _ => panic!("expected parsed mouse release"),
        }
    }

    #[test]
    fn test_parse_raw_sgr_mouse_sequence_incomplete_prefix() {
        assert!(matches!(
            parse_raw_sgr_mouse_sequence(b"\x1b[<65;188"),
            RawSgrMouseParse::Incomplete
        ));
    }

    #[test]
    fn test_parse_raw_sgr_mouse_sequence_rejects_non_mouse_csi() {
        assert!(matches!(
            parse_raw_sgr_mouse_sequence(b"\x1b[A"),
            RawSgrMouseParse::Invalid
        ));
    }

    #[test]
    fn test_mouse_scroll_routes_by_pane_geometry_when_regions_are_empty() {
        let mut mux = Mux::new(10, 80);
        let mut left = brehon_mux::Pane::director("left", 5, 40).expect("left pane");
        let mut supervisor =
            brehon_mux::Pane::director("supervisor", 5, 40).expect("supervisor pane");
        for idx in 0..30 {
            left.append_output(format!("left {idx}\r\n").as_bytes())
                .expect("append left");
            supervisor
                .append_output(format!("supervisor {idx}\r\n").as_bytes())
                .expect("append supervisor");
        }
        mux.add_pane(left);
        mux.add_pane(supervisor);
        mux.focus("supervisor");

        let mut group_tab = GroupTab::Workers;
        let mut selected_worker = 0;
        let mut selected_panel = 0;
        let mut selected_member = Vec::new();
        let worker_ids = vec!["left".to_string()];
        let all_reviewer_ids = Vec::new();
        let panels = Vec::new();
        let supervisor_id = Some("supervisor".to_string());
        let active_left_id = Some("left".to_string());
        let mut expanded_epics = std::collections::HashSet::new();
        let mut expanded_activity_rows = std::collections::HashSet::new();
        let mut selection = None;
        let mut pending_down = None;
        let mut task_detail = None;
        let mut advisor_room_view = AdvisorRoomViewState::default();
        let mut research_room_view = ResearchRoomViewState::default();
        let mut dashboard_agent_list = DashboardAgentListState::default();
        let mut dashboard_task_list = DashboardTaskListState::default();
        let structured_mode = std::collections::HashSet::new();
        let mut structured_scroll_offsets = std::collections::HashMap::new();
        let mut external_terminal_tab_request = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;

        let stale = handle_mouse_input(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
            &[],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &all_reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded_epics,
            &mut expanded_activity_rows,
            Rect::new(0, 0, 40, 10),
            Rect::new(40, 0, 40, 10),
            &mut selection,
            &mut pending_down,
            &mut task_detail,
            &mut advisor_room_view,
            &mut research_room_view,
            &mut dashboard_agent_list,
            &mut dashboard_task_list,
            &structured_mode,
            &mut structured_scroll_offsets,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(!stale);
        assert!(mux.get("left").unwrap().display_scroll_offset() > 0);
        assert_eq!(mux.get("supervisor").unwrap().display_scroll_offset(), 0);
        assert_eq!(mux.focused_id(), Some("supervisor"));
    }

    #[test]
    fn test_mouse_scroll_structured_gateway_uses_structured_scroll_offset() {
        let mut mux = Mux::new(10, 80);
        let mut left = make_custom_worker_pane("left", "codex-worker");
        left.ensure_activity_buffer();
        assert!(left.is_gateway_backed());
        let supervisor = brehon_mux::Pane::director("supervisor", 5, 40).expect("supervisor pane");
        mux.add_pane(left);
        mux.add_pane(supervisor);
        mux.focus("supervisor");

        let mut group_tab = GroupTab::Workers;
        let mut selected_worker = 0;
        let mut selected_panel = 0;
        let mut selected_member = Vec::new();
        let worker_ids = vec!["left".to_string()];
        let all_reviewer_ids = Vec::new();
        let panels = Vec::new();
        let supervisor_id = Some("supervisor".to_string());
        let active_left_id = Some("left".to_string());
        let mut expanded_epics = std::collections::HashSet::new();
        let mut expanded_activity_rows = std::collections::HashSet::new();
        let mut selection = None;
        let mut pending_down = None;
        let mut task_detail = None;
        let mut advisor_room_view = AdvisorRoomViewState::default();
        let mut research_room_view = ResearchRoomViewState::default();
        let mut dashboard_agent_list = DashboardAgentListState::default();
        let mut dashboard_task_list = DashboardTaskListState::default();
        let mut structured_mode = std::collections::HashSet::new();
        structured_mode.insert("left".to_string());
        let mut structured_scroll_offsets = std::collections::HashMap::new();
        let mut external_terminal_tab_request = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;

        let stale = handle_mouse_input(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
            &[],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &all_reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded_epics,
            &mut expanded_activity_rows,
            Rect::new(0, 0, 40, 10),
            Rect::new(40, 0, 40, 10),
            &mut selection,
            &mut pending_down,
            &mut task_detail,
            &mut advisor_room_view,
            &mut research_room_view,
            &mut dashboard_agent_list,
            &mut dashboard_task_list,
            &structured_mode,
            &mut structured_scroll_offsets,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(!stale);
        assert_eq!(structured_scroll_offsets.get("left"), Some(&3));
        assert_eq!(mux.get("left").unwrap().display_scroll_offset(), 0);
        assert_eq!(mux.focused_id(), Some("supervisor"));

        let _ = handle_mouse_input(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
            &[],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &all_reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded_epics,
            &mut expanded_activity_rows,
            Rect::new(0, 0, 40, 10),
            Rect::new(40, 0, 40, 10),
            &mut selection,
            &mut pending_down,
            &mut task_detail,
            &mut advisor_room_view,
            &mut research_room_view,
            &mut dashboard_agent_list,
            &mut dashboard_task_list,
            &structured_mode,
            &mut structured_scroll_offsets,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(!structured_scroll_offsets.contains_key("left"));
    }

    #[test]
    fn test_mouse_scroll_panesmith_supervisor_uses_viewport_offset() {
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
        assert!(mux.is_panesmith_managed("codex-supervisor"));
        mux.focus("codex-supervisor");

        let mut group_tab = GroupTab::Workers;
        let mut selected_worker = 0;
        let mut selected_panel = 0;
        let mut selected_member = Vec::new();
        let worker_ids = Vec::new();
        let all_reviewer_ids = Vec::new();
        let panels = Vec::new();
        let supervisor_id = Some("codex-supervisor".to_string());
        let active_left_id = None;
        let mut expanded_epics = std::collections::HashSet::new();
        let mut expanded_activity_rows = std::collections::HashSet::new();
        let mut selection = None;
        let mut pending_down = None;
        let mut task_detail = None;
        let mut advisor_room_view = AdvisorRoomViewState::default();
        let mut research_room_view = ResearchRoomViewState::default();
        let mut dashboard_agent_list = DashboardAgentListState::default();
        let mut dashboard_task_list = DashboardTaskListState::default();
        let structured_mode = std::collections::HashSet::new();
        let mut structured_scroll_offsets = std::collections::HashMap::new();
        let mut external_terminal_tab_request = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;

        let stale = handle_mouse_input(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp,
                column: 45,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
            &[],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &all_reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded_epics,
            &mut expanded_activity_rows,
            Rect::new(0, 0, 40, 10),
            Rect::new(40, 0, 40, 10),
            &mut selection,
            &mut pending_down,
            &mut task_detail,
            &mut advisor_room_view,
            &mut research_room_view,
            &mut dashboard_agent_list,
            &mut dashboard_task_list,
            &structured_mode,
            &mut structured_scroll_offsets,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(!stale);
        assert_eq!(structured_scroll_offsets.get("codex-supervisor"), Some(&3));
        assert_eq!(
            mux.get("codex-supervisor").unwrap().display_scroll_offset(),
            0
        );

        let _ = handle_mouse_input(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 45,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
            &[],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &all_reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded_epics,
            &mut expanded_activity_rows,
            Rect::new(0, 0, 40, 10),
            Rect::new(40, 0, 40, 10),
            &mut selection,
            &mut pending_down,
            &mut task_detail,
            &mut advisor_room_view,
            &mut research_room_view,
            &mut dashboard_agent_list,
            &mut dashboard_task_list,
            &structured_mode,
            &mut structured_scroll_offsets,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(!structured_scroll_offsets.contains_key("codex-supervisor"));
    }

    #[cfg(unix)]
    #[test]
    fn test_panesmith_styled_scrollback_survives_brehon_scroll_render_path() {
        use ratatui::{backend::TestBackend, style::Modifier, Terminal};

        const SUPERVISOR_ID: &str = "styled-supervisor";

        let _serial = SERIAL_PANESMITH_STYLE_TEST
            .lock()
            .expect("serial Panesmith style test lock poisoned");
        let project_root = tempfile::tempdir().expect("tempdir");
        let script = concat!(
            "printf '\\033[31mHIST_RED\\033[0m\\r\\n'; ",
            "printf '\\033[1;32mHIST_GREEN_BOLD\\033[0m\\r\\n'; ",
            "i=1; while [ $i -le 12 ]; do printf 'plain tail %02d\\r\\n' \"$i\"; i=$((i + 1)); done; ",
            "printf '\\033[1;32mLIVE_GREEN_BOLD\\033[0m\\r\\n'; ",
            "sleep 30"
        );
        let mut mux = Mux::factory(brehon_mux::MuxConfig {
            cwd: project_root.path().to_path_buf(),
            workers: 0,
            supervisor_name: SUPERVISOR_ID.to_string(),
            supervisor_cli: custom_interactive_agent("styled-test-agent", "sh", &["-c", script]),
            worker_cli: brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            include_director: false,
            rows: 6,
            cols: 100,
            ..Default::default()
        })
        .expect("create mux");
        assert!(mux.is_panesmith_managed(SUPERVISOR_ID));
        mux.focus(SUPERVISOR_ID);

        wait_for_panesmith_text(&mut mux, SUPERVISOR_ID, "LIVE_GREEN_BOLD");
        wait_for_panesmith_scrollback_text(&mut mux, SUPERVISOR_ID, "HIST_RED");

        let live_style = panesmith_snapshot_style_for_text(
            mux.panesmith_snapshot(SUPERVISOR_ID)
                .expect("live snapshot"),
            "LIVE_GREEN_BOLD",
        )
        .expect("live styled row");
        assert_eq!(live_style.fg, Some(panesmith::ColorSpec::Indexed(2)));
        assert!(live_style.attrs.bold);

        let scrollback = mux.panesmith_scrollback(SUPERVISOR_ID).expect("scrollback");
        let red_history_style =
            panesmith_scrollback_style_for_text(scrollback, "HIST_RED").expect("red history row");
        assert_eq!(red_history_style.fg, Some(panesmith::ColorSpec::Indexed(1)));
        let green_history_style =
            panesmith_scrollback_style_for_text(scrollback, "HIST_GREEN_BOLD")
                .expect("green history row");
        assert_eq!(
            green_history_style.fg,
            Some(panesmith::ColorSpec::Indexed(2))
        );
        assert!(green_history_style.attrs.bold);

        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal
            .draw(|frame| {
                let expanded = std::collections::HashSet::new();
                let _ = render_pane_in_area_with_activity_regions(
                    frame,
                    Rect::new(0, 0, 100, 10),
                    &mux,
                    SUPERVISOR_ID,
                    true,
                    None,
                    false,
                    &expanded,
                    None,
                    None,
                );
            })
            .unwrap();
        let live_cell_pos =
            buffer_text_cell(terminal.backend().buffer(), "LIVE_GREEN_BOLD").expect("live cell");
        let live_cell = terminal
            .backend()
            .buffer()
            .cell(live_cell_pos)
            .expect("live cell");
        assert_eq!(live_cell.fg, Color::Indexed(2));
        assert!(live_cell.modifier.contains(Modifier::BOLD));

        let mut structured_scroll_offsets = std::collections::HashMap::new();
        for _ in 0..8 {
            scroll_supervisor_with_brehon_mouse_path(
                &mut mux,
                SUPERVISOR_ID,
                &mut structured_scroll_offsets,
                MouseEventKind::ScrollUp,
            );
        }
        let scroll_offset = structured_scroll_offsets
            .get(SUPERVISOR_ID)
            .copied()
            .expect("Brehon mouse scroll should move Panesmith viewport away from tail");
        assert!(scroll_offset > 0);

        let mut scrolled_terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        scrolled_terminal
            .draw(|frame| {
                let expanded = std::collections::HashSet::new();
                let _ = render_pane_in_area_with_activity_regions(
                    frame,
                    Rect::new(0, 0, 100, 10),
                    &mux,
                    SUPERVISOR_ID,
                    true,
                    None,
                    false,
                    &expanded,
                    Some(scroll_offset),
                    None,
                );
            })
            .unwrap();
        let buffer = scrolled_terminal.backend().buffer();
        let red_cell_pos = buffer_text_cell(buffer, "HIST_RED").expect("red scrollback cell");
        let red_cell = buffer.cell(red_cell_pos).expect("red scrollback cell");
        assert_eq!(red_cell.fg, Color::Indexed(1));

        let green_cell_pos =
            buffer_text_cell(buffer, "HIST_GREEN_BOLD").expect("green scrollback cell");
        let green_cell = buffer.cell(green_cell_pos).expect("green scrollback cell");
        assert_eq!(green_cell.fg, Color::Indexed(2));
        assert!(green_cell.modifier.contains(Modifier::BOLD));

        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(mux.shutdown_all());
    }

    #[cfg(unix)]
    fn wait_for_panesmith_text(mux: &mut Mux, pane_id: &str, needle: &str) {
        wait_for_panesmith_condition(mux, |mux| {
            mux.panesmith_snapshot(pane_id).is_some_and(|snapshot| {
                panesmith_snapshot_style_for_text(snapshot, needle).is_some()
            })
        });
    }

    #[cfg(unix)]
    fn wait_for_panesmith_scrollback_text(mux: &mut Mux, pane_id: &str, needle: &str) {
        wait_for_panesmith_condition(mux, |mux| {
            let _ = mux.refresh_panesmith_scrollback(pane_id);
            mux.panesmith_scrollback(pane_id).is_some_and(|scrollback| {
                panesmith_scrollback_style_for_text(scrollback, needle).is_some()
            })
        });
    }

    #[cfg(unix)]
    fn wait_for_panesmith_condition(mux: &mut Mux, mut predicate: impl FnMut(&mut Mux) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            mux.poll_batch();
            if predicate(mux) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Panesmith pane content"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn scroll_supervisor_with_brehon_mouse_path(
        mux: &mut Mux,
        supervisor_id: &str,
        structured_scroll_offsets: &mut std::collections::HashMap<String, usize>,
        kind: MouseEventKind,
    ) {
        let mut group_tab = GroupTab::Workers;
        let mut selected_worker = 0;
        let mut selected_panel = 0;
        let mut selected_member = Vec::new();
        let worker_ids = Vec::new();
        let all_reviewer_ids = Vec::new();
        let panels = Vec::new();
        let supervisor_id = Some(supervisor_id.to_string());
        let active_left_id = None;
        let mut expanded_epics = std::collections::HashSet::new();
        let mut expanded_activity_rows = std::collections::HashSet::new();
        let mut selection = None;
        let mut pending_down = None;
        let mut task_detail = None;
        let mut advisor_room_view = AdvisorRoomViewState::default();
        let mut research_room_view = ResearchRoomViewState::default();
        let mut dashboard_agent_list = DashboardAgentListState::default();
        let mut dashboard_task_list = DashboardTaskListState::default();
        let structured_mode = std::collections::HashSet::new();
        let mut external_terminal_tab_request = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;

        let _ = handle_mouse_input(
            crossterm::event::MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
            &[],
            mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &all_reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded_epics,
            &mut expanded_activity_rows,
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 100, 10),
            &mut selection,
            &mut pending_down,
            &mut task_detail,
            &mut advisor_room_view,
            &mut research_room_view,
            &mut dashboard_agent_list,
            &mut dashboard_task_list,
            &structured_mode,
            structured_scroll_offsets,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );
    }

    #[test]
    fn test_read_live_reviewer_panels_preserves_configured_groups_when_leases_cross_panels() {
        let temp = tempfile::tempdir().expect("tempdir");
        let review_panels_dir = temp.path().join("runtime").join("review-panels");
        std::fs::create_dir_all(&review_panels_dir).expect("create review-panels dir");
        std::fs::write(
            review_panels_dir.join("primary.json"),
            serde_json::json!({
                "panel_id": "primary",
                "members": [
                    { "slot_agent": "claude-reviewer", "reviewer": "claude-r2" },
                    { "slot_agent": "codex", "reviewer": "codex-r2" },
                    { "slot_agent": "gemini", "reviewer": "gemini-r2" }
                ]
            })
            .to_string(),
        )
        .expect("write panel lease");

        let fallback = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec![
                    "claude-r1".to_string(),
                    "codex-r1".to_string(),
                    "gemini-r1".to_string(),
                ],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec![
                    "claude-r2".to_string(),
                    "codex-r2".to_string(),
                    "gemini-r2".to_string(),
                ],
            },
        ];

        let panels = read_live_reviewer_panels(temp.path(), &fallback);
        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0].name, "primary");
        assert_eq!(
            panels[0].members,
            vec![
                "claude-r1".to_string(),
                "codex-r1".to_string(),
                "gemini-r1".to_string(),
            ]
        );
        assert_eq!(panels[1].name, "secondary");
        assert_eq!(
            panels[1].members,
            vec![
                "claude-r2".to_string(),
                "codex-r2".to_string(),
                "gemini-r2".to_string(),
            ]
        );
    }

    #[test]
    fn test_read_live_reviewer_panels_keeps_unleased_configured_panels_stable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let review_panels_dir = temp.path().join("runtime").join("review-panels");
        std::fs::create_dir_all(&review_panels_dir).expect("create review-panels dir");
        std::fs::write(
            review_panels_dir.join("primary.json"),
            serde_json::json!({
                "panel_id": "primary",
                "members": [
                    { "slot_agent": "claude-reviewer", "reviewer": "claude-r1" },
                    { "slot_agent": "codex-reviewer", "reviewer": "codex-r1" },
                    { "slot_agent": "gemini-reviewer", "reviewer": "codex-r2" }
                ]
            })
            .to_string(),
        )
        .expect("write primary panel lease");
        std::fs::write(
            review_panels_dir.join("secondary.json"),
            serde_json::json!({
                "panel_id": "secondary",
                "members": [
                    { "slot_agent": "claude-reviewer", "reviewer": "claude-r2" },
                    { "slot_agent": "codex-reviewer", "reviewer": "codex-r3" },
                    { "slot_agent": "gemini-reviewer", "reviewer": "gemini-r2" }
                ]
            })
            .to_string(),
        )
        .expect("write secondary panel lease");

        let fallback = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec![
                    "claude-r1".to_string(),
                    "codex-r1".to_string(),
                    "gemini-r1".to_string(),
                ],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec![
                    "claude-r2".to_string(),
                    "codex-r2".to_string(),
                    "gemini-r2".to_string(),
                ],
            },
            ReviewerPanel {
                name: "tertiary".to_string(),
                members: vec![
                    "claude-r3".to_string(),
                    "codex-r3".to_string(),
                    "gemini-r3".to_string(),
                ],
            },
        ];

        let panels = read_live_reviewer_panels(temp.path(), &fallback);
        assert_eq!(panels.len(), 3);
        assert_eq!(panels[0].name, "primary");
        assert_eq!(
            panels[0].members,
            vec![
                "claude-r1".to_string(),
                "codex-r1".to_string(),
                "gemini-r1".to_string(),
            ]
        );
        assert_eq!(panels[1].name, "secondary");
        assert_eq!(
            panels[1].members,
            vec![
                "claude-r2".to_string(),
                "codex-r2".to_string(),
                "gemini-r2".to_string(),
            ]
        );
        assert_eq!(panels[2].name, "tertiary");
        assert_eq!(
            panels[2].members,
            vec![
                "claude-r3".to_string(),
                "codex-r3".to_string(),
                "gemini-r3".to_string(),
            ]
        );

        let all_members: Vec<String> = panels
            .iter()
            .flat_map(|panel| panel.members.iter().cloned())
            .collect();
        let unique_members: std::collections::HashSet<String> =
            all_members.iter().cloned().collect();
        assert_eq!(all_members.len(), unique_members.len());
    }

    #[test]
    fn test_read_live_reviewer_panels_ignores_stale_leased_member_for_configured_panel() {
        let temp = tempfile::tempdir().expect("tempdir");
        let review_panels_dir = temp.path().join("runtime").join("review-panels");
        std::fs::create_dir_all(&review_panels_dir).expect("create review-panels dir");
        std::fs::write(
            review_panels_dir.join("tertiary.json"),
            serde_json::json!({
                "panel_id": "tertiary",
                "members": [
                    { "slot_agent": "claude-reviewer", "reviewer": "old-claude" },
                    { "slot_agent": "codex-reviewer", "reviewer": "codex-r3" },
                    { "slot_agent": "glm-reviewer", "reviewer": "glm-r3" }
                ]
            })
            .to_string(),
        )
        .expect("write panel lease");

        let fallback = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec![
                    "claude-r1".to_string(),
                    "codex-r1".to_string(),
                    "glm-r1".to_string(),
                ],
            },
            ReviewerPanel {
                name: "tertiary".to_string(),
                members: vec![
                    "claude-r3".to_string(),
                    "codex-r3".to_string(),
                    "glm-r3".to_string(),
                ],
            },
        ];

        let panels = read_live_reviewer_panels(temp.path(), &fallback);
        let tertiary = panels
            .iter()
            .find(|panel| panel.name == "tertiary")
            .expect("tertiary panel");
        assert_eq!(
            tertiary.members,
            vec![
                "claude-r3".to_string(),
                "codex-r3".to_string(),
                "glm-r3".to_string(),
            ]
        );
    }

    #[test]
    fn test_read_live_reviewer_panels_keeps_configured_panel_size_when_lease_is_sparse() {
        let temp = tempfile::tempdir().expect("tempdir");
        let review_panels_dir = temp.path().join("runtime").join("review-panels");
        std::fs::create_dir_all(&review_panels_dir).expect("create review-panels dir");
        std::fs::write(
            review_panels_dir.join("tertiary.json"),
            serde_json::json!({
                "panel_id": "tertiary",
                "members": [
                    { "slot_agent": "claude-reviewer", "reviewer": "claude-r3" }
                ]
            })
            .to_string(),
        )
        .expect("write sparse panel lease");

        let fallback = vec![ReviewerPanel {
            name: "tertiary".to_string(),
            members: vec![
                "claude-r3".to_string(),
                "codex-r3".to_string(),
                "glm-r3".to_string(),
            ],
        }];

        let panels = read_live_reviewer_panels(temp.path(), &fallback);
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].name, "tertiary");
        assert_eq!(
            panels[0].members,
            vec![
                "claude-r3".to_string(),
                "codex-r3".to_string(),
                "glm-r3".to_string(),
            ]
        );
    }

    #[test]
    fn test_read_live_reviewer_panels_ignores_duplicate_leased_member_for_configured_panel() {
        let temp = tempfile::tempdir().expect("tempdir");
        let review_panels_dir = temp.path().join("runtime").join("review-panels");
        std::fs::create_dir_all(&review_panels_dir).expect("create review-panels dir");
        std::fs::write(
            review_panels_dir.join("primary.json"),
            serde_json::json!({
                "panel_id": "primary",
                "members": [
                    { "slot_agent": "claude-reviewer", "reviewer": "shared-r1" },
                    { "slot_agent": "codex-reviewer", "reviewer": "codex-r1" }
                ]
            })
            .to_string(),
        )
        .expect("write primary panel lease");
        std::fs::write(
            review_panels_dir.join("secondary.json"),
            serde_json::json!({
                "panel_id": "secondary",
                "members": [
                    { "slot_agent": "claude-reviewer", "reviewer": "shared-r1" },
                    { "slot_agent": "codex-reviewer", "reviewer": "codex-r2" }
                ]
            })
            .to_string(),
        )
        .expect("write secondary panel lease");

        let fallback = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec!["shared-r1".to_string(), "codex-r1".to_string()],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec!["claude-r2".to_string(), "codex-r2".to_string()],
            },
        ];

        let panels = read_live_reviewer_panels(temp.path(), &fallback);
        let secondary = panels
            .iter()
            .find(|panel| panel.name == "secondary")
            .expect("secondary panel");
        assert_eq!(
            secondary.members,
            vec!["claude-r2".to_string(), "codex-r2".to_string()]
        );

        let all_members: Vec<String> = panels
            .iter()
            .flat_map(|panel| panel.members.iter().cloned())
            .collect();
        let unique_members: std::collections::HashSet<String> =
            all_members.iter().cloned().collect();
        assert_eq!(all_members.len(), unique_members.len());
    }

    #[test]
    fn test_apply_reviewer_selection_state_preserves_selected_member_by_identity() {
        let initial_panels = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec![
                    "alpha".to_string(),
                    "bravo".to_string(),
                    "charlie".to_string(),
                ],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec![
                    "delta".to_string(),
                    "echo".to_string(),
                    "foxtrot".to_string(),
                ],
            },
        ];
        let mut selection = ReviewerSelectionState::default();
        let mut selected_panel = 0usize;
        let mut selected_member = vec![2usize, 1usize];
        capture_reviewer_selection_state(
            &initial_panels,
            selected_panel,
            &selected_member,
            &mut selection,
        );

        let reordered_panels = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec![
                    "charlie".to_string(),
                    "alpha".to_string(),
                    "bravo".to_string(),
                ],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec![
                    "foxtrot".to_string(),
                    "delta".to_string(),
                    "echo".to_string(),
                ],
            },
        ];

        apply_reviewer_selection_state(
            &reordered_panels,
            &mut selection,
            &mut selected_panel,
            &mut selected_member,
        );

        assert_eq!(selected_panel, 0);
        assert_eq!(selected_member, vec![0, 2]);
    }

    #[test]
    fn test_apply_reviewer_selection_state_follows_selected_member_to_new_panel() {
        let initial_panels = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec!["alpha".to_string(), "bravo".to_string()],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec!["charlie".to_string(), "delta".to_string()],
            },
        ];
        let mut selection = ReviewerSelectionState::default();
        let mut selected_panel = 0usize;
        let mut selected_member = vec![1usize, 0usize];
        capture_reviewer_selection_state(
            &initial_panels,
            selected_panel,
            &selected_member,
            &mut selection,
        );

        let moved_panels = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec!["alpha".to_string(), "charlie".to_string()],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec!["bravo".to_string(), "delta".to_string()],
            },
        ];

        apply_reviewer_selection_state(
            &moved_panels,
            &mut selection,
            &mut selected_panel,
            &mut selected_member,
        );

        assert_eq!(selected_panel, 1);
        assert_eq!(selected_member, vec![0, 0]);
    }

    #[test]
    fn test_should_drop_stale_review_prompt_when_task_no_longer_in_review() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-stale", "merged");
        write_test_review_state(temp.path(), "T-stale", "REV-stale", "approved");

        let prompt = "Review request REV-stale for task T-stale: Example\nRound: 1\n";
        assert!(should_drop_stale_review_prompt(temp.path(), prompt));
    }

    #[test]
    fn test_should_keep_active_review_prompt_when_review_is_collecting() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-active", "in_review");
        write_test_review_state(temp.path(), "T-active", "REV-active", "collecting");

        let prompt = "Review request REV-active for task T-active: Example\nRound: 1\n";
        assert!(!should_drop_stale_review_prompt(temp.path(), prompt));
    }

    #[test]
    fn test_should_drop_timed_out_review_prompt_when_review_already_resolved() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-timeout", "changes_requested");
        write_test_review_state(temp.path(), "T-timeout", "REV-timeout", "changes_requested");

        let prompt = "Review REV-timeout for task T-timeout timed out and is no longer active. Stop reviewing this round.\n";
        assert!(should_drop_stale_review_prompt(temp.path(), prompt));
    }

    #[test]
    fn test_should_keep_timed_out_review_prompt_if_review_still_collecting() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-active", "in_review");
        write_test_review_state(temp.path(), "T-active", "REV-active", "collecting");

        let prompt = "Review REV-active for task T-active timed out and is no longer active. Stop reviewing this round.\n";
        assert!(!should_drop_stale_review_prompt(temp.path(), prompt));
    }

    #[test]
    fn test_should_retry_missing_pane_prompt_after_failure() {
        assert!(!should_dead_letter_prompt_after_failure(
            "Ping worker",
            "Pane not found: quick-kit-72"
        ));
        assert!(!should_dead_letter_prompt_after_failure(
            "Ping worker",
            "Session dead-pane not found"
        ));
    }

    #[test]
    fn test_rewrite_stale_consolidated_report_when_task_already_merged() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task = serde_json::json!({
            "task_id": "T-stale",
            "status": "merged",
            "title": "Task T-stale",
            "closed_by": "claude-code",
            "closed_at": "2026-04-05T22:38:13Z",
            "merged_commit": "d9ff2e0",
            "merged_branch": "main"
        });
        std::fs::write(
            tasks_dir.join("T-stale.json"),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();

        let prompt = "Review complete for task T-stale\n\
                      Review ID: REV-stale\n\
                      Outcome: APPROVED\n\
                      Round: 2/3\n\
                      Task gate now: approved\n\
                      Completion mode: merge\n\n\
                      Task approved. The task status is now 'approved'. SUPERVISOR: You must perform the terminal merge.\n\
                      task action=close id=T-stale\n";

        let rewritten = rewrite_stale_consolidated_report(temp.path(), prompt).unwrap();
        assert!(rewritten.contains("Late consolidated review report for task T-stale"));
        assert!(rewritten.contains("Review ID: REV-stale"));
        assert!(rewritten.contains("Outcome: APPROVED"));
        assert!(rewritten.contains("Current task status: merged"));
        assert!(rewritten.contains("Already handled by claude-code at 2026-04-05T22:38:13Z"));
        assert!(rewritten.contains("Verified merged commit: d9ff2e0"));
        assert!(rewritten.contains("Merged branch: main"));
        assert!(rewritten.contains("No action required."));
        assert!(!rewritten.contains("task action=close"));
    }

    #[test]
    fn test_rewrite_stale_consolidated_report_ignores_non_terminal_task() {
        let temp = tempfile::tempdir().unwrap();
        write_test_task(temp.path(), "T-active", "approved");

        let prompt = "Review complete for task T-active\n\
                      Review ID: REV-active\n\
                      Outcome: APPROVED\n\
                      Round: 1/3\n";

        assert!(rewrite_stale_consolidated_report(temp.path(), prompt).is_none());
    }

    #[test]
    fn test_read_task_files_loads_detail_fields() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task = serde_json::json!({
            "task_id": "T-detail",
            "title": "Detail-rich task",
            "status": "in_progress",
            "assignee": "worker-1",
            "task_type": "task",
            "description": "A full task brief",
            "priority": "critical",
            "percent": 70,
            "completion_mode": "merge",
            "merge_target": "epic/feature-context",
            "integration_branch": "epic/feature-context",
            "integration_worktree": "/tmp/worktrees/epic-feature-context",
            "activity": "testing",
            "notes": "Tests are passing",
            "blockers": "Waiting on reviewer feedback",
            "dependencies": ["T-base-1", "T-base-2"],
            "blocked_by": ["T-base-1"],
            "created_at": "2026-04-06T01:00:00Z",
            "updated_at": "2026-04-06T01:05:00Z",
            "review_feedback": {
                "review_id": "REV-detail",
                "round": 2,
                "outcome": "changes_requested",
                "threshold_reason": "Blocking regression still reachable from normal input",
                "blocking": [{
                    "description": "Guard the failpoint behind test-only input",
                    "file": "crates/brehon-tui/src/run.rs",
                    "line": 412,
                    "severity": "blocking",
                    "suggestion": "Use a cfg(test) hook instead of runtime input"
                }],
                "suggestions": [{
                    "description": "Add one more regression around stale prompt rendering",
                    "severity": "suggestion"
                }],
                "dissent": ["Reviewer beta preferred a narrower helper split"],
                "evaluated_at": "2026-04-06T01:06:00Z"
            },
            "acceptance_criteria": ["Criterion A", "Criterion B"],
            "file_hints": ["crates/brehon-tui/src/run.rs"],
            "test_requirements": ["cargo test -p brehon-tui"],
            "plan_steps": ["Implement modal"],
            "implementation_notes": "Do not break dashboard clicks"
        });
        std::fs::write(
            tasks_dir.join("T-detail.json"),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
        let reviews_dir = temp.path().join("runtime").join("reviews").join("T-detail");
        std::fs::create_dir_all(&reviews_dir).unwrap();
        std::fs::write(
            reviews_dir.join("state.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-detail",
                "status": "changes_requested",
                "current_round": 2,
                "current_review_id": "REV-detail",
                "panel_id": "primary",
                "panel": ["reviewer-a", "reviewer-b"],
            }))
            .unwrap(),
        )
        .unwrap();
        let review_panels_dir = temp.path().join("runtime").join("review-panels");
        std::fs::create_dir_all(&review_panels_dir).unwrap();
        std::fs::write(
            review_panels_dir.join("primary.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "panel_id": "primary",
                "task_id": "T-detail",
                "review_id": "REV-detail",
                "round": 2,
                "members": [
                    {"slot_agent": "codex", "reviewer": "reviewer-a"},
                    {"slot_agent": "gemini", "reviewer": "reviewer-b"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        let loaded = tasks.iter().find(|task| task.id == "T-detail").unwrap();
        assert_eq!(loaded.description, "A full task brief");
        assert_eq!(loaded.priority.as_deref(), Some("critical"));
        assert_eq!(loaded.percent, Some(70));
        assert_eq!(loaded.merge_target.as_deref(), Some("epic/feature-context"));
        assert_eq!(
            loaded.integration_branch.as_deref(),
            Some("epic/feature-context")
        );
        assert_eq!(
            loaded.integration_worktree.as_deref(),
            Some("/tmp/worktrees/epic-feature-context")
        );
        assert_eq!(loaded.activity.as_deref(), Some("testing"));
        assert_eq!(loaded.notes.as_deref(), Some("Tests are passing"));
        assert_eq!(
            loaded.blockers.as_deref(),
            Some("Waiting on reviewer feedback")
        );
        assert_eq!(loaded.dependencies, vec!["T-base-1", "T-base-2"]);
        assert_eq!(loaded.blocked_by, vec!["T-base-1"]);
        assert_eq!(loaded.review_id.as_deref(), Some("REV-detail"));
        assert_eq!(loaded.review_status.as_deref(), Some("changes_requested"));
        assert_eq!(loaded.review_round, Some(2));
        assert_eq!(loaded.review_panel_id.as_deref(), Some("primary"));
        assert_eq!(
            loaded.review_panel_members,
            vec!["reviewer-a", "reviewer-b"]
        );
        assert_eq!(
            loaded.review_panel_lease_state.as_deref(),
            Some("leased_waiting_for_revision")
        );
        assert_eq!(
            loaded.review_feedback_outcome.as_deref(),
            Some("changes_requested")
        );
        assert_eq!(loaded.review_feedback_blocking.len(), 1);
        assert_eq!(loaded.review_feedback_suggestions.len(), 1);
        assert_eq!(loaded.review_feedback_dissent.len(), 1);
        assert_eq!(loaded.acceptance_criteria.len(), 2);
        assert_eq!(loaded.file_hints, vec!["crates/brehon-tui/src/run.rs"]);
        assert_eq!(loaded.plan_steps, vec!["Implement modal"]);
        assert_eq!(
            loaded.implementation_notes.as_deref(),
            Some("Do not break dashboard clicks")
        );
    }

    #[test]
    fn test_build_task_detail_lines_renders_review_context_and_feedback() {
        let mut task = make_task(
            "T-review",
            "Review Detail",
            "changes_requested",
            "task",
            None,
        );
        task.review_id = Some("REV-review".to_string());
        task.review_status = Some("changes_requested".to_string());
        task.review_round = Some(2);
        task.review_panel_id = Some("primary".to_string());
        task.review_panel_members = vec!["reviewer-a".to_string(), "reviewer-b".to_string()];
        task.review_panel_lease_state = Some("leased_waiting_for_revision".to_string());
        task.review_feedback_outcome = Some("changes_requested".to_string());
        task.review_feedback_threshold_reason =
            Some("Blocking issue still reachable from normal input".to_string());
        task.review_feedback_blocking = vec![
            "[crates/brehon-tui/src/run.rs:412] Guard the failpoint behind test-only input"
                .to_string(),
        ];
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![task.clone()],
            events: Vec::new(),
            brehon_root: None,
        };

        let rendered = lines_to_string(&build_task_detail_lines(&task, &dashboard));
        assert!(rendered.contains("Review Context"));
        assert!(rendered.contains("Panel primary"), "rendered: {rendered}");
        assert!(
            rendered.contains("Lease leased_waiting_for_revision"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("Review Feedback"));
        assert!(rendered.contains("Blocking Findings"));
        assert!(rendered.contains("Guard the failpoint behind test-only input"));
    }

    #[test]
    fn test_build_task_detail_lines_renders_research_context() {
        let mut task = make_task("T-research", "Research Detail", "in_progress", "task", None);
        task.research_context = vec![ResearchContextInfo {
            artifact_id: "RCH-T-research-specs-001".to_string(),
            role: "normative_requirements".to_string(),
            title: "PFCP requirements".to_string(),
            summary: "Heartbeat and recovery timestamp handling are required.".to_string(),
            artifact_path: Some(
                ".brehon/runtime/research/T-research/RCH-T-research-specs-001/brief.md".to_string(),
            ),
            confidence: Some("medium".to_string()),
        }];
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![task.clone()],
            events: Vec::new(),
            brehon_root: None,
        };

        let rendered = lines_to_string(&build_task_detail_lines(&task, &dashboard));
        assert!(rendered.contains("Research Context"));
        assert!(rendered.contains("RCH-T-research-specs-001"));
        assert!(rendered.contains("PFCP requirements"));
        assert!(rendered.contains("Heartbeat and recovery timestamp"));
    }

    #[test]
    fn test_build_task_detail_lines_renders_dependencies_and_blocked_by() {
        let mut blocked = make_task("T-blocked", "Blocked Task", "blocked", "task", Some("E-1"));
        blocked.dependencies = vec!["T-a".to_string(), "T-b".to_string()];
        blocked.blocked_by = vec!["T-b".to_string()];

        let mut dep_a = make_task("T-a", "Dependency A", "closed", "task", Some("E-1"));
        dep_a.assignee = None;
        let mut dep_b = make_task("T-b", "Dependency B", "in_progress", "task", Some("E-1"));
        dep_b.assignee = Some("worker-2".to_string());

        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![
                make_task("E-1", "Epic", "pending", "epic", None),
                dep_a,
                dep_b,
                blocked.clone(),
            ],
            events: Vec::new(),
            brehon_root: None,
        };

        let rendered = lines_to_string(&build_task_detail_lines(&blocked, &dashboard));
        assert!(rendered.contains("Dependencies"));
        assert!(rendered.contains("T-a — Dependency A [closed]"));
        assert!(rendered.contains("T-b — Dependency B [in_progress]"));
        assert!(rendered.contains("Blocked By"));
        assert!(rendered.contains("T-b — Dependency B [in_progress]"));
    }

    #[test]
    fn test_build_task_detail_lines_renders_integration_conflict_section() {
        let mut task = make_task(
            "T-conflict",
            "Conflict Task",
            "changes_requested",
            "task",
            None,
        );
        task.integration_conflict_owner = Some("supervisor".to_string());
        task.integration_conflict_source = Some("approved_integration".to_string());
        task.integration_conflict_merge_target = Some("epic/phase-2".to_string());
        task.integration_conflict_reviewed_commit = Some("deadbeef".to_string());
        task.integration_conflict_previous_worker = Some("worker-9".to_string());
        task.integration_conflict_conflicting_files =
            vec!["Cargo.toml".to_string(), "Cargo.lock".to_string()];

        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![task.clone()],
            events: Vec::new(),
            brehon_root: None,
        };

        let rendered = lines_to_string(&build_task_detail_lines(&task, &dashboard));
        assert!(rendered.contains("Integration Conflict"));
        assert!(rendered.contains("Owner supervisor"));
        assert!(rendered.contains("Merge Target epic/phase-2"));
        assert!(rendered.contains("Reviewed Commit deadbeef"));
        assert!(rendered.contains("Previous Worker worker-9"));
        assert!(rendered.contains("Cargo.toml"));
    }

    #[test]
    fn test_task_dashboard_hint_prefers_blocked_by_chain() {
        let mut blocker = make_task("T-base", "Base Task", "in_progress", "task", None);
        blocker.assignee = Some("worker-2".to_string());
        let mut blocked = make_task("T-blocked", "Blocked Task", "blocked", "task", None);
        blocked.blocked_by = vec!["T-base".to_string(), "T-other".to_string()];
        let tasks_by_id = std::collections::HashMap::from([
            (blocker.id.as_str(), &blocker),
            (blocked.id.as_str(), &blocked),
        ]);

        let hint = task_dashboard_hint(&blocked, &tasks_by_id).unwrap();
        assert!(hint.contains("waiting on"));
        assert!(hint.contains("T-base"));
        assert!(hint.contains("+1"));
    }

    #[test]
    fn test_task_dashboard_hint_surfaces_supervisor_integration_conflict() {
        let mut task = make_task(
            "T-conflict",
            "Conflict Task",
            "changes_requested",
            "task",
            None,
        );
        task.integration_conflict_owner = Some("supervisor".to_string());
        task.integration_conflict_conflicting_files =
            vec!["Cargo.toml".to_string(), "Cargo.lock".to_string()];

        let tasks_by_id = std::collections::HashMap::from([(task.id.as_str(), &task)]);
        let hint = task_dashboard_hint(&task, &tasks_by_id).unwrap();
        assert!(hint.contains("supervisor conflict"));
        assert!(hint.contains("Cargo.toml"));
        assert!(hint.contains("+1"));
    }

    #[test]
    fn test_compute_display_status_distinguishes_review_ready_from_active_review() {
        let mut queued = make_task("T-queued", "Queued Review", "in_review", "task", None);
        queued.review_panel_lease_state = Some("awaiting_panel".to_string());
        assert_eq!(compute_display_status(&queued), "review_ready");

        let mut collecting = make_task("T-active", "Active Review", "in_review", "task", None);
        collecting.review_status = Some("collecting".to_string());
        collecting.review_panel_lease_state = Some("collecting".to_string());
        assert_eq!(compute_display_status(&collecting), "in_review");
    }

    #[test]
    fn test_compute_display_status_surfaces_supervisor_integration_conflict() {
        let mut task = make_task(
            "T-conflict",
            "Conflict Task",
            "changes_requested",
            "task",
            None,
        );
        task.integration_conflict_owner = Some("supervisor".to_string());
        assert_eq!(compute_display_status(&task), "integration_conflict");
    }

    #[test]
    fn test_compute_display_status_prefers_integrated_over_epic_branch_merge_metadata() {
        let mut integrated =
            make_task("T-integrated", "Integrated Subtask", "closed", "task", None);
        integrated.integration_status = Some("integrated".to_string());
        integrated.merged_branch = Some("epic/test-feature".to_string());
        integrated.merged_commit = Some("abc1234".to_string());
        assert_eq!(compute_display_status(&integrated), "integrated");
    }

    #[test]
    fn test_compute_display_status_does_not_mask_reopened_pending_task_as_integrated() {
        let mut reopened = make_task("T-reopened", "Reopened Task", "pending", "task", None);
        reopened.integration_status = Some("integrated".to_string());
        assert_eq!(compute_display_status(&reopened), "pending");
    }

    #[test]
    fn test_compute_supervisor_dispatch_frontier_detects_pending_work_with_idle_workers() {
        let mut pending = make_task("T-pending", "Pending Task", "pending", "task", None);
        pending.assignee = None;
        let mut busy = make_task("T-busy", "Busy Task", "in_progress", "task", None);
        busy.assignee = Some("worker-1".to_string());
        let tasks = vec![pending, busy];
        let sessions = std::collections::HashMap::from([
            (
                "worker-1".to_string(),
                (
                    "worker".to_string(),
                    "sess-1".to_string(),
                    "now".to_string(),
                ),
            ),
            (
                "worker-2".to_string(),
                (
                    "worker".to_string(),
                    "sess-2".to_string(),
                    "now".to_string(),
                ),
            ),
        ]);

        let frontier =
            compute_supervisor_dispatch_frontier(&tasks, &sessions).expect("dispatch frontier");
        assert_eq!(frontier.idle_workers, vec!["worker-2".to_string()]);
        assert_eq!(frontier.pending_tasks, vec!["T-pending".to_string()]);
        assert!(frontier.integration_conflict_tasks.is_empty());
        assert!(frontier.changes_requested_tasks.is_empty());
        assert!(frontier.review_ready_tasks.is_empty());
        assert!(frontier.approved_tasks.is_empty());
    }

    #[test]
    fn test_compute_supervisor_dispatch_frontier_requires_idle_worker_for_pending_only() {
        let mut pending = make_task("T-pending", "Pending Task", "pending", "task", None);
        pending.assignee = None;
        let mut busy = make_task("T-busy", "Busy Task", "in_progress", "task", None);
        busy.assignee = Some("worker-1".to_string());
        let tasks = vec![pending, busy];
        let sessions = std::collections::HashMap::from([(
            "worker-1".to_string(),
            (
                "worker".to_string(),
                "sess-1".to_string(),
                "now".to_string(),
            ),
        )]);

        assert!(compute_supervisor_dispatch_frontier(&tasks, &sessions).is_none());
    }

    #[test]
    fn test_compute_supervisor_dispatch_frontier_surfaces_review_and_integration_queues() {
        let mut review_ready = make_task("T-review", "Review Task", "review_ready", "task", None);
        review_ready.assignee = Some("worker-1".to_string());
        let mut approved = make_task("T-approved", "Approved Task", "approved", "task", None);
        approved.assignee = None;
        approved.completion_mode = Some("merge".to_string());
        let tasks = vec![review_ready, approved];
        let sessions = std::collections::HashMap::from([(
            "worker-1".to_string(),
            (
                "worker".to_string(),
                "sess-1".to_string(),
                "now".to_string(),
            ),
        )]);

        let frontier =
            compute_supervisor_dispatch_frontier(&tasks, &sessions).expect("dispatch frontier");
        assert!(frontier.idle_workers.is_empty());
        assert!(frontier.integration_conflict_tasks.is_empty());
        assert_eq!(frontier.review_ready_tasks, vec!["T-review".to_string()]);
        assert_eq!(frontier.approved_tasks, vec!["T-approved".to_string()]);

        let message = build_supervisor_dispatch_nudge_message(&frontier, false);
        assert!(message.contains("T-review"));
        assert!(message.contains("T-approved"));
        assert!(message.contains("task action=ready"));
        assert!(message.contains("Re-run"));
    }

    #[test]
    fn test_build_supervisor_dispatch_nudge_message_headless_uses_direct_tools() {
        let tasks = vec![make_task(
            "T-review",
            "Review Task",
            "review_ready",
            "task",
            None,
        )];
        let sessions = std::collections::HashMap::new();
        let frontier =
            compute_supervisor_dispatch_frontier(&tasks, &sessions).expect("dispatch frontier");
        let message = build_supervisor_dispatch_nudge_message(&frontier, true);
        assert!(message.contains("T-review"));
        assert!(!message.contains("Re-run"));
        assert!(message.contains("Do not wait for operator confirmation"));
    }

    #[test]
    fn test_build_supervisor_dispatch_nudge_message_headless_conflicts_uses_direct_tools() {
        let mut conflict = make_task(
            "T-conflict",
            "Conflict Task",
            "changes_requested",
            "task",
            None,
        );
        conflict.integration_conflict_owner = Some("supervisor".to_string());
        conflict.integration_conflict_source = Some("approved_integration".to_string());
        let tasks = vec![conflict];
        let sessions = std::collections::HashMap::new();

        let frontier =
            compute_supervisor_dispatch_frontier(&tasks, &sessions).expect("dispatch frontier");
        let message = build_supervisor_dispatch_nudge_message(&frontier, true);
        assert!(message.contains("T-conflict"));
        assert!(message.contains("task action=conflicts"));
        assert!(!message.contains("Re-run"));
        assert!(message.contains("Do not wait for operator confirmation"));
    }

    #[test]
    fn test_compute_supervisor_dispatch_frontier_surfaces_supervisor_conflicts_first() {
        let mut conflict = make_task(
            "T-conflict",
            "Conflict Task",
            "changes_requested",
            "task",
            None,
        );
        conflict.integration_conflict_owner = Some("supervisor".to_string());
        conflict.integration_conflict_source = Some("approved_integration".to_string());
        let mut busy = make_task("T-busy", "Busy Task", "in_progress", "task", None);
        busy.assignee = Some("worker-1".to_string());
        let tasks = vec![conflict, busy];
        let sessions = std::collections::HashMap::from([(
            "worker-1".to_string(),
            (
                "worker".to_string(),
                "sess-1".to_string(),
                "now".to_string(),
            ),
        )]);

        let frontier =
            compute_supervisor_dispatch_frontier(&tasks, &sessions).expect("dispatch frontier");
        assert!(frontier.idle_workers.is_empty());
        assert_eq!(
            frontier.integration_conflict_tasks,
            vec!["T-conflict".to_string()]
        );
        assert!(frontier.changes_requested_tasks.is_empty());

        let message = build_supervisor_dispatch_nudge_message(&frontier, false);
        assert!(message.contains("T-conflict"));
        assert!(message.contains("task action=conflicts"));
        assert!(message.contains("before requesting review"));
    }

    #[test]
    fn test_compute_supervisor_dispatch_frontier_surfaces_dependency_cleared_blocked_reassignment()
    {
        let blocker = make_task("T-blocker", "Closed blocker", "closed", "task", None);
        let mut dependent = make_task(
            "T-dependent",
            "Dependency-cleared blocked task",
            "blocked",
            "task",
            None,
        );
        dependent.assignee = Some("dead-worker".to_string());
        dependent.percent = Some(10);
        dependent.activity = Some("reading".to_string());
        dependent.dependencies = vec!["T-blocker".to_string()];
        dependent.blockers = Some(
            "Imported dependency DAG is not yet satisfied: T-blocker is still InProgress."
                .to_string(),
        );
        let tasks = vec![blocker, dependent];
        let sessions = std::collections::HashMap::from([(
            "worker-1".to_string(),
            (
                "worker".to_string(),
                "sess-1".to_string(),
                "now".to_string(),
            ),
        )]);

        let frontier =
            compute_supervisor_dispatch_frontier(&tasks, &sessions).expect("dispatch frontier");
        assert_eq!(frontier.idle_workers, vec!["worker-1".to_string()]);
        assert_eq!(frontier.pending_tasks, vec!["T-dependent".to_string()]);
    }

    #[test]
    fn test_compute_display_status_keeps_blocked_task_over_stale_integrated_metadata() {
        let mut blocked = make_task("T-blocked", "Blocked Task", "blocked", "task", None);
        blocked.integration_status = Some("integrated".to_string());
        assert_eq!(compute_display_status(&blocked), "blocked");
    }

    #[test]
    fn test_compute_display_status_marks_mainline_merge_as_merged() {
        let mut merged = make_task("T-merged", "Merged Task", "closed", "task", None);
        merged.merged_branch = Some("main".to_string());
        merged.merged_commit = Some("def5678".to_string());
        assert_eq!(compute_display_status(&merged), "merged");
    }

    #[test]
    fn test_read_task_files_marks_in_review_without_review_state_as_awaiting_panel() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("T-awaiting.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "task_id": "T-awaiting",
                "title": "Awaiting review seat",
                "status": "in_review",
                "task_type": "task"
            }))
            .unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        let task = tasks.iter().find(|task| task.id == "T-awaiting").unwrap();
        assert_eq!(compute_display_status(task), "review_ready");
        assert_eq!(
            task.review_panel_lease_state.as_deref(),
            Some("awaiting_panel")
        );
        let rendered = lines_to_string(&build_task_detail_lines(
            task,
            &DashboardData {
                agents: Vec::new(),
                tasks: tasks.clone(),
                events: Vec::new(),
                brehon_root: None,
            },
        ));
        assert!(
            rendered.contains("review_ready"),
            "should contain review_ready status"
        );
        assert!(
            rendered.contains("awaiting_panel"),
            "should contain awaiting_panel lease"
        );
    }

    #[test]
    fn test_read_task_files_hides_terminal_epic_without_active_children() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let merged_epic = serde_json::json!({
            "task_id": "E-old",
            "title": "Old Epic",
            "status": "merged",
            "task_type": "epic"
        });
        let merged_subtask = serde_json::json!({
            "task_id": "T-old-1",
            "title": "Old Subtask",
            "status": "merged",
            "task_type": "task",
            "parent_id": "E-old"
        });
        let active_epic = serde_json::json!({
            "task_id": "E-new",
            "title": "Active Epic",
            "status": "pending",
            "task_type": "epic"
        });

        std::fs::write(
            tasks_dir.join("E-old.json"),
            serde_json::to_string_pretty(&merged_epic).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("T-old-1.json"),
            serde_json::to_string_pretty(&merged_subtask).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("E-new.json"),
            serde_json::to_string_pretty(&active_epic).unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        assert!(tasks.iter().any(|task| task.id == "E-new"));
        assert!(!tasks.iter().any(|task| task.id == "E-old"));
        assert!(!tasks.iter().any(|task| task.id == "T-old-1"));
    }

    #[test]
    fn test_read_task_files_keeps_terminal_subtasks_under_active_epic() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let active_epic = serde_json::json!({
            "task_id": "E-live",
            "title": "Live Epic",
            "status": "pending",
            "task_type": "epic"
        });
        let merged_subtask = serde_json::json!({
            "task_id": "T-done",
            "title": "Completed Subtask",
            "status": "merged",
            "task_type": "task",
            "parent_id": "E-live"
        });
        let active_subtask = serde_json::json!({
            "task_id": "T-open",
            "title": "Open Subtask",
            "status": "in_progress",
            "task_type": "task",
            "parent_id": "E-live"
        });

        std::fs::write(
            tasks_dir.join("E-live.json"),
            serde_json::to_string_pretty(&active_epic).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("T-done.json"),
            serde_json::to_string_pretty(&merged_subtask).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("T-open.json"),
            serde_json::to_string_pretty(&active_subtask).unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        assert!(tasks.iter().any(|task| task.id == "E-live"));
        assert!(tasks.iter().any(|task| task.id == "T-done"));
        assert!(tasks.iter().any(|task| task.id == "T-open"));
    }

    #[test]
    fn test_read_task_files_keeps_active_initiative_hierarchy_visible() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let initiative = serde_json::json!({
            "task_id": "I-live",
            "title": "Live Initiative",
            "status": "pending",
            "task_type": "initiative"
        });
        let epic = serde_json::json!({
            "task_id": "E-live",
            "title": "Live Epic",
            "status": "pending",
            "task_type": "epic",
            "parent_id": "I-live"
        });
        let task = serde_json::json!({
            "task_id": "T-open",
            "title": "Open Task",
            "status": "in_progress",
            "task_type": "task",
            "parent_id": "E-live"
        });

        std::fs::write(
            tasks_dir.join("I-live.json"),
            serde_json::to_string_pretty(&initiative).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("E-live.json"),
            serde_json::to_string_pretty(&epic).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("T-open.json"),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        assert!(tasks.iter().any(|task| task.id == "I-live"));
        assert!(tasks.iter().any(|task| task.id == "E-live"));
        assert!(tasks.iter().any(|task| task.id == "T-open"));
    }

    #[test]
    fn test_read_task_files_keeps_merged_phase_under_active_initiative() {
        let temp = tempfile::tempdir().unwrap();
        let tasks_dir = temp.path().join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let initiative = serde_json::json!({
            "task_id": "I-live",
            "title": "Live Initiative",
            "status": "pending",
            "task_type": "initiative"
        });
        let merged_phase = serde_json::json!({
            "task_id": "E-done",
            "title": "Merged Phase",
            "status": "merged",
            "task_type": "epic",
            "parent_id": "I-live"
        });
        let open_phase = serde_json::json!({
            "task_id": "E-open",
            "title": "Open Phase",
            "status": "pending",
            "task_type": "epic",
            "parent_id": "I-live"
        });

        std::fs::write(
            tasks_dir.join("I-live.json"),
            serde_json::to_string_pretty(&initiative).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("E-done.json"),
            serde_json::to_string_pretty(&merged_phase).unwrap(),
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("E-open.json"),
            serde_json::to_string_pretty(&open_phase).unwrap(),
        )
        .unwrap();

        let tasks = read_task_files(temp.path());
        assert!(tasks.iter().any(|task| task.id == "I-live"));
        assert!(tasks.iter().any(|task| task.id == "E-done"));
        assert!(tasks.iter().any(|task| task.id == "E-open"));
    }

    #[test]
    fn test_render_dashboard_tasks_emits_detail_regions_for_rows() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![
                make_task("E-1", "Epic", "pending", "epic", None),
                make_task("T-1", "Child", "in_progress", "task", Some("E-1")),
                make_task("T-2", "Orphan", "pending", "task", None),
            ],
            events: Vec::new(),
            brehon_root: None,
        };
        let expanded = std::collections::HashSet::from(["E-1".to_string()]);
        let mut regions = Vec::new();
        let mut state = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                regions = render_dashboard_tasks(
                    frame,
                    Rect::new(0, 0, 120, 18),
                    &dashboard,
                    &expanded,
                    &mut state,
                );
            })
            .unwrap();

        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::EpicToggle("E-1".to_string())));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("E-1".to_string())));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("T-1".to_string())));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("T-2".to_string())));

        let epic_detail_idx = regions
            .iter()
            .position(|region| region.target == ClickTarget::TaskDetail("E-1".to_string()))
            .expect("epic detail region");
        let epic_toggle_idx = regions
            .iter()
            .position(|region| region.target == ClickTarget::EpicToggle("E-1".to_string()))
            .expect("epic toggle region");
        let epic_detail = &regions[epic_detail_idx];
        let epic_toggle = &regions[epic_toggle_idx];
        assert!(
            epic_detail_idx < epic_toggle_idx,
            "ID/detail region must win over full-row toggle"
        );
        assert!(epic_toggle.rect.width > epic_detail.rect.width);
    }

    #[test]
    fn test_render_dashboard_auto_expands_new_containers_once() {
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mux = Mux::new(24, 80);
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![
                make_task("E-1", "Epic", "pending", "epic", None),
                make_task("T-1", "Child", "in_progress", "task", Some("E-1")),
            ],
            events: Vec::new(),
            brehon_root: None,
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();
        let mut regions = Vec::new();

        terminal
            .draw(|frame| {
                regions = render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 24),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        assert!(expanded.contains("E-1"));
        assert!(task_list.known_container_ids.contains("E-1"));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("T-1".to_string())));

        expanded.remove("E-1");
        terminal
            .draw(|frame| {
                regions = render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 24),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        assert!(!expanded.contains("E-1"));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::EpicToggle("E-1".to_string())));
        assert!(!regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("T-1".to_string())));
    }

    #[test]
    fn test_render_dashboard_agents_separates_cli_and_provider_columns() {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_custom_worker_pane(
            "soft-ram-33",
            "codex-ollama-worker",
        ));

        let dashboard = DashboardData {
            agents: vec![AgentInfo {
                name: "soft-ram-33".to_string(),
                role: "worker".to_string(),
                cli: "codex".to_string(),
                session_id: Some("sess-1".to_string()),
                last_seen_at: None,
            }],
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: None,
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let header_row = (0..buffer.area.height)
            .map(|row| buffer_row_string(buffer, row))
            .find(|row| row.contains("Agent") && row.contains("Provider"))
            .expect("dashboard header row");
        assert!(header_row.contains("CLI"));
        assert!(!header_row.contains("Role"));

        let agent_row = (0..buffer.area.height)
            .map(|row| buffer_row_string(buffer, row))
            .find(|row| row.contains("soft-ram-33"))
            .expect("dashboard agent row");
        assert!(agent_row.contains("▰"));
        assert!(agent_row.contains("codex"));
        assert!(agent_row.contains("codex-ollama-worker"));
        assert!(agent_row.contains("starting"));
        assert!(!agent_row.contains("codex-ollama-workerstarting"));
    }

    #[test]
    fn test_render_dashboard_agents_shows_runtime_token_counter() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(brehon_root.join("runtime")).unwrap();
        std::fs::write(
            brehon_types::runtime_stability_counters_path(&brehon_root),
            serde_json::to_string(&brehon_types::StabilityCounters {
                tokens_used: 12_345,
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_custom_worker_pane("soft-ram-33", "codex-worker"));

        let dashboard = DashboardData {
            agents: vec![AgentInfo {
                name: "soft-ram-33".to_string(),
                role: "worker".to_string(),
                cli: "codex".to_string(),
                session_id: Some("sess-1".to_string()),
                last_seen_at: None,
            }],
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: Some(brehon_root),
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        let rows: Vec<String> = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect();
        assert!(rows.iter().any(|row| row.contains("tokens 12,345")));
    }

    #[test]
    fn test_render_dashboard_runtime_status_emits_approval_click_regions() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        let generated_at_ms = test_unix_timestamp_ms();
        let command = brehon_types::RuntimeCommand {
            command_id: "cmd-approval".to_string(),
            target: brehon_types::RuntimeCommandTarget {
                session_id: "session-1".to_string(),
                pane_id: Some("worker-1".to_string()),
                generation: Some(1),
            },
            issued_at_ms: 1,
            kind: brehon_types::RuntimeCommandKind::ResetPane {
                reason: "operator".to_string(),
            },
        };
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": generated_at_ms,
                "running": true,
                "metrics": {
                    "published_events": 2,
                    "routed_commands": 3,
                    "rejected_commands": 0,
                    "deferred_commands": 1,
                    "pending_approvals": 1,
                    "audit_write_errors": 0
                },
                "registry_count": 4,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": [{
                        "session_id": "session-1",
                        "pane_id": "worker-1",
                        "generation": 1,
                        "state": "ready",
                        "kind": "worker",
                        "source": "headless",
                        "title": "Worker 1",
                        "last_event_ms": 1,
                        "last_output_ms": 2
                    }]
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": [{
                        "approval_id": "approval-1",
                        "reason": "operation requires explicit approval",
                        "command": command
                    }]
                },
                "sidecar": {
                    "detection_running": true,
                    "workflow_running": true
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": true
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mux = Mux::new(24, 80);
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: Some(brehon_root),
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();
        let mut regions = Vec::new();

        terminal
            .draw(|frame| {
                regions = render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 24),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        assert!(regions.iter().any(|region| {
            region.target
                == ClickTarget::RuntimeApproval {
                    approval_id: "approval-1".to_string(),
                    session_id: "session-1".to_string(),
                    approved: true,
                }
        }));
        assert!(regions.iter().any(|region| {
            region.target
                == ClickTarget::RuntimeApproval {
                    approval_id: "approval-1".to_string(),
                    session_id: "session-1".to_string(),
                    approved: false,
                }
        }));
        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Runtime"));
        assert!(rendered.contains("host=headless"));
        assert!(rendered.contains("mode=preview"));
        assert!(rendered.contains("host_panes=1"));
        assert!(rendered.contains("panes=4"));
        assert!(rendered.contains("pane_owner=mux"));
        assert!(rendered.contains("cmds=3/2"));
        assert!(rendered.contains("deferred=1"));
        assert!(rendered.contains("approval-1"));
    }

    #[test]
    fn test_render_dashboard_runtime_status_disables_stale_approval_actions() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        let command = brehon_types::RuntimeCommand {
            command_id: "cmd-approval".to_string(),
            target: brehon_types::RuntimeCommandTarget {
                session_id: "session-1".to_string(),
                pane_id: Some("worker-1".to_string()),
                generation: Some(1),
            },
            issued_at_ms: 1,
            kind: brehon_types::RuntimeCommandKind::ResetPane {
                reason: "operator".to_string(),
            },
        };
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": 1,
                "running": true,
                "metrics": {
                    "published_events": 2,
                    "routed_commands": 3,
                    "rejected_commands": 0,
                    "deferred_commands": 1,
                    "pending_approvals": 1,
                    "audit_write_errors": 0
                },
                "registry_count": 1,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": [{
                        "session_id": "session-1",
                        "pane_id": "worker-1",
                        "generation": 1,
                        "state": "ready",
                        "kind": "worker",
                        "last_event_ms": 1
                    }]
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": [{
                        "approval_id": "approval-1",
                        "reason": "operation requires explicit approval",
                        "command": command
                    }]
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": false
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mux = Mux::new(24, 80);
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: Some(brehon_root),
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();
        let mut regions = Vec::new();

        terminal
            .draw(|frame| {
                regions = render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 24),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("daemon heartbeat stale"));
        assert!(rendered.contains("runtime approvals disabled until heartbeat refreshes"));
        assert!(!rendered.contains("[approve]"));
        assert!(!regions
            .iter()
            .any(|region| matches!(region.target, ClickTarget::RuntimeApproval { .. })));
    }

    #[test]
    fn test_render_dashboard_runtime_status_shows_registry_preview_without_approvals() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": 1,
                "running": true,
                "metrics": {
                    "published_events": 2,
                    "routed_commands": 3,
                    "rejected_commands": 0,
                    "deferred_commands": 1,
                    "pending_approvals": 0,
                    "audit_write_errors": 0
                },
                "registry_count": 1,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": [{
                        "session_id": "session-1",
                        "pane_id": "worker-1",
                        "generation": 3,
                        "state": "ready",
                        "kind": "worker",
                        "source": "headless",
                        "title": "Worker 1",
                        "last_event_ms": 1,
                        "last_output_ms": 2
                    }]
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": []
                },
                "sidecar": {
                    "detection_running": true,
                    "workflow_running": true
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": false
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mux = Mux::new(24, 80);
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: Some(brehon_root),
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 24),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("daemon heartbeat stale"));
        assert!(rendered.contains("host=headless"));
        assert!(rendered.contains("observation=off"));
        assert!(rendered.contains("session-1/worker-1"));
        assert!(rendered.contains("worker ready"));
        assert!(rendered.contains("source=headless"));
        assert!(rendered.contains("title=\"Worker 1\""));
        assert!(rendered.contains("no pending approvals"));
    }

    #[test]
    fn test_render_runtime_view_expanded_breaks_out_host_and_registry() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": test_unix_timestamp_ms(),
                "running": true,
                "metrics": {
                    "published_events": 5,
                    "routed_commands": 3,
                    "rejected_commands": 0,
                    "deferred_commands": 0,
                    "pending_approvals": 0,
                    "audit_write_errors": 0
                },
                "registry_count": 2,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": [
                        {
                            "session_id": "session-1",
                            "pane_id": "worker-1",
                            "generation": 1,
                            "state": "ready",
                            "kind": "worker",
                            "source": "mux",
                            "title": "Worker 1",
                            "last_event_ms": 1,
                            "last_output_ms": 2
                        },
                        {
                            "session_id": "session-1",
                            "pane_id": "host-preview",
                            "generation": 1,
                            "state": "ready",
                            "kind": "shell",
                            "source": "headless",
                            "title": "host-preview",
                            "last_event_ms": 1,
                            "last_output_ms": 2
                        }
                    ]
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": []
                },
                "sidecar": {
                    "detection_running": true,
                    "workflow_running": true
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": true,
                    "command_routing": "mux",
                    "capabilities": {
                        "source": "headless",
                        "interactive_pty": true,
                        "scrollback": true,
                        "structured_activity": true,
                        "absolute_resize": false,
                        "out_of_process_lifecycle": true,
                        "replay": false
                    },
                    "session_name": "brehon-session"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();

        terminal
            .draw(|frame| {
                render_runtime_view(
                    frame,
                    Rect::new(0, 0, 120, 24),
                    Some(&brehon_root),
                    None,
                    &[],
                );
            })
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Terminal Host"));
        assert!(rendered.contains("Pane Registry"));
        assert!(rendered.contains("Pending Approvals"));
        assert!(rendered.contains("kind=headless experimental"));
        assert!(rendered.contains("mode=preview"));
        assert!(rendered.contains("host_panes=1"));
        assert!(rendered.contains("commands=mux"));
        assert!(rendered.contains("pane_owner=mux"));
        assert!(rendered.contains("capabilities source=headless"));
        assert!(rendered.contains("resize=unsupported"));
        assert!(rendered.contains("promotion=blocked"));
        assert!(rendered.contains("daemon commands still route to mux"));
        assert!(rendered.contains("agent panes are still mux-owned"));
        assert!(rendered.contains("terminal host does not advertise absolute resize"));
        assert!(rendered.contains("session=brehon-session"));
        assert!(rendered.contains("Pane"));
        assert!(rendered.contains("Owner"));
        assert!(rendered.contains("Output"));
        assert!(rendered.contains("Title"));
        assert!(rendered.contains("session-1/host-preview"));
        assert!(rendered.contains("session-1/worker-1"));
        assert!(rendered.contains("no pending approvals"));
    }

    #[test]
    fn test_render_runtime_view_expanded_shows_recent_commands_and_health() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": 1,
                "running": true,
                "metrics": {
                    "published_events": 5,
                    "routed_commands": 4,
                    "rejected_commands": 1,
                    "deferred_commands": 0,
                    "pending_approvals": 0,
                    "audit_write_errors": 0
                },
                "registry_count": 1,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": [{
                        "session_id": "session-1",
                        "pane_id": "worker-1",
                        "generation": 1,
                        "state": "ready",
                        "kind": "worker",
                        "source": "headless",
                        "title": "Worker 1",
                        "last_event_ms": 1,
                        "last_output_ms": 2
                    }]
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": []
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": true,
                    "command_routing": "terminal_host"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let updated_at_ms = test_unix_timestamp_ms().saturating_sub(50);
        let recent_runtime_commands = vec![RuntimeCommandActivity {
            command_id: "cmd-reset".to_string(),
            label: "reset".to_string(),
            target: Some("worker-1".to_string()),
            status: "applied".to_string(),
            message: Some("reset applied".to_string()),
            issued_at_ms: updated_at_ms.saturating_sub(10),
            updated_at_ms,
        }];
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();

        terminal
            .draw(|frame| {
                render_runtime_view(
                    frame,
                    Rect::new(0, 0, 120, 30),
                    Some(&brehon_root),
                    None,
                    &recent_runtime_commands,
                );
            })
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("health=stale heartbeat"));
        assert!(rendered.contains("Recent Commands"));
        assert!(rendered.contains("cmd applied reset target=worker-1"));
        assert!(rendered.contains("reset applied"));
    }

    #[test]
    fn test_render_dashboard_runtime_status_shows_recent_command_activity() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": test_unix_timestamp_ms(),
                "running": true,
                "metrics": {
                    "published_events": 2,
                    "routed_commands": 3,
                    "rejected_commands": 0,
                    "deferred_commands": 0,
                    "pending_approvals": 0,
                    "audit_write_errors": 0
                },
                "registry_count": 0,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": []
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": []
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": true
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let updated_at_ms = test_unix_timestamp_ms().saturating_sub(25);
        let recent_runtime_commands = vec![RuntimeCommandActivity {
            command_id: "cmd-input".to_string(),
            label: "terminal-input".to_string(),
            target: Some("worker-1".to_string()),
            status: "pending".to_string(),
            message: None,
            issued_at_ms: updated_at_ms,
            updated_at_ms,
        }];
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let mux = Mux::new(24, 80);
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: Some(brehon_root),
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 24),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &recent_runtime_commands,
                    0,
                );
            })
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("cmd pending terminal-input target=worker-1"));
    }

    #[test]
    fn test_runtime_output_age_label_formats_relative_time() {
        assert_eq!(runtime_output_age_label_at(10_000, None), "-");
        assert_eq!(runtime_output_age_label_at(10_000, Some(9_950)), "50ms ago");
        assert_eq!(runtime_output_age_label_at(10_000, Some(8_500)), "1s ago");
        assert_eq!(runtime_output_age_label_at(120_000, Some(60_000)), "1m ago");
        assert_eq!(
            runtime_output_age_label_at(8_000_000, Some(3_000_000)),
            "1h ago"
        );
    }

    #[test]
    fn test_runtime_terminal_host_attach_command_is_absent_without_external_host() {
        let status = RuntimeTerminalHostDashboardStatus {
            kind: brehon_types::RuntimeTerminalHostKind::Headless,
            experimental: true,
            observation_running: true,
            command_routing: RuntimeTerminalHostCommandRoutingDashboard::Mux,
            pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
            agent_factory: RuntimeTerminalHostAgentFactoryRoutingDashboard::Mux,
            capabilities: None,
            promotion_readiness: RuntimeTerminalHostPromotionReadinessDashboard::default(),
            session_name: Some("brehon-session".to_string()),
            socket_name: None,
            socket_dir: None,
            binary_path: None,
            diagnostics: Vec::new(),
        };

        assert!(runtime_terminal_host_attach_command(&status).is_none());
    }

    #[test]
    fn test_render_runtime_view_expanded_emits_approval_click_regions() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let status_dir = brehon_root.join("runtime").join("daemon");
        std::fs::create_dir_all(&status_dir).unwrap();
        let command = brehon_types::RuntimeCommand {
            command_id: "cmd-approval".to_string(),
            target: brehon_types::RuntimeCommandTarget {
                session_id: "session-1".to_string(),
                pane_id: Some("worker-1".to_string()),
                generation: Some(1),
            },
            issued_at_ms: 1,
            kind: brehon_types::RuntimeCommandKind::ResetPane {
                reason: "operator".to_string(),
            },
        };
        std::fs::write(
            status_dir.join("current.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "generated_at_ms": test_unix_timestamp_ms(),
                "running": true,
                "metrics": {
                    "published_events": 5,
                    "routed_commands": 3,
                    "rejected_commands": 0,
                    "deferred_commands": 1,
                    "pending_approvals": 1,
                    "audit_write_errors": 0
                },
                "registry_count": 1,
                "registry": {
                    "generated_at_ms": 1,
                    "panes": [{
                        "session_id": "session-1",
                        "pane_id": "worker-1",
                        "generation": 1,
                        "state": "ready",
                        "kind": "worker",
                        "source": "mux",
                        "last_event_ms": 1
                    }]
                },
                "approvals": {
                    "generated_at_ms": 1,
                    "approvals": [{
                        "approval_id": "approval-1",
                        "reason": "operation requires explicit approval",
                        "command": command
                    }]
                },
                "terminal_host": {
                    "kind": "headless",
                    "experimental": true,
                    "observation_running": true,
                    "command_routing": "mux"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let mut regions = Vec::new();

        terminal
            .draw(|frame| {
                regions = render_runtime_view(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    Some(&brehon_root),
                    None,
                    &[],
                );
            })
            .unwrap();

        assert!(regions.iter().any(|region| {
            region.target
                == ClickTarget::RuntimeApproval {
                    approval_id: "approval-1".to_string(),
                    session_id: "session-1".to_string(),
                    approved: true,
                }
        }));
        assert!(regions.iter().any(|region| {
            region.target
                == ClickTarget::RuntimeApproval {
                    approval_id: "approval-1".to_string(),
                    session_id: "session-1".to_string(),
                    approved: false,
                }
        }));
        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Pending Approvals"));
        assert!(rendered.contains("approval-1"));
        assert!(rendered.contains("[approve]"));
        assert!(rendered.contains("[deny]"));
    }

    #[test]
    fn test_runtime_registry_summary_includes_source_counts() {
        let status = RuntimeDaemonDashboardStatus {
            generated_at_ms: 1,
            running: true,
            metrics: RuntimeDaemonDashboardMetrics::default(),
            registry_count: 2,
            registry: RuntimePaneRegistryDashboardSnapshot {
                panes: vec![
                    RuntimePaneDashboardInfo {
                        session_id: "session-1".to_string(),
                        pane_id: "worker-1".to_string(),
                        generation: 1,
                        state: brehon_types::RuntimePaneState::Ready,
                        kind: brehon_types::RuntimePaneKind::Worker,
                        source: Some(brehon_types::RuntimeSource::Mux),
                        title: None,
                        last_output_ms: None,
                        exit_code: None,
                        exit_reason: None,
                        blocked: None,
                    },
                    RuntimePaneDashboardInfo {
                        session_id: "session-1".to_string(),
                        pane_id: "host-preview".to_string(),
                        generation: 1,
                        state: brehon_types::RuntimePaneState::Ready,
                        kind: brehon_types::RuntimePaneKind::Shell,
                        source: Some(brehon_types::RuntimeSource::Headless),
                        title: None,
                        last_output_ms: None,
                        exit_code: None,
                        exit_reason: None,
                        blocked: None,
                    },
                ],
            },
            approvals: RuntimeApprovalDashboardSnapshot::default(),
            sidecar: None,
            terminal_host: Some(RuntimeTerminalHostDashboardStatus {
                kind: brehon_types::RuntimeTerminalHostKind::Headless,
                experimental: true,
                observation_running: true,
                command_routing: RuntimeTerminalHostCommandRoutingDashboard::Mux,
                pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
                agent_factory: RuntimeTerminalHostAgentFactoryRoutingDashboard::Mux,
                capabilities: None,
                promotion_readiness: RuntimeTerminalHostPromotionReadinessDashboard::default(),
                session_name: Some("brehon-session".to_string()),
                socket_name: None,
                socket_dir: None,
                binary_path: None,
                diagnostics: Vec::new(),
            }),
        };

        let summary = runtime_registry_summary(&status, None);

        assert!(summary.contains("panes=2"));
        assert!(summary.contains("ready=2"));
        assert!(summary.contains("sources=headless:1,mux:1"));
    }

    #[test]
    fn test_runtime_registry_summary_includes_live_backend_owner_counts() {
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
        let status = RuntimeDaemonDashboardStatus {
            generated_at_ms: 1,
            running: true,
            metrics: RuntimeDaemonDashboardMetrics::default(),
            registry_count: 1,
            registry: RuntimePaneRegistryDashboardSnapshot {
                panes: vec![RuntimePaneDashboardInfo {
                    session_id: "session-1".to_string(),
                    pane_id: "codex-supervisor".to_string(),
                    generation: 1,
                    state: brehon_types::RuntimePaneState::Ready,
                    kind: brehon_types::RuntimePaneKind::Supervisor,
                    source: Some(brehon_types::RuntimeSource::Mux),
                    title: None,
                    last_output_ms: None,
                    exit_code: None,
                    exit_reason: None,
                    blocked: None,
                }],
            },
            approvals: RuntimeApprovalDashboardSnapshot::default(),
            sidecar: None,
            terminal_host: None,
        };

        let summary = runtime_registry_summary(&status, Some(&mux));

        assert!(summary.contains("sources=mux:1"));
        assert!(summary.contains("owners=panesmith:1"));
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(mux.shutdown_all());
    }

    #[test]
    fn test_runtime_terminal_host_summary_includes_headless_identity() {
        let status = RuntimeTerminalHostDashboardStatus {
            kind: brehon_types::RuntimeTerminalHostKind::Headless,
            experimental: true,
            observation_running: true,
            command_routing: RuntimeTerminalHostCommandRoutingDashboard::Mux,
            pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
            agent_factory: RuntimeTerminalHostAgentFactoryRoutingDashboard::Mux,
            capabilities: Some(brehon_types::TerminalHostCapabilities {
                source: brehon_types::RuntimeSource::Headless,
                interactive_pty: true,
                scrollback: true,
                structured_activity: true,
                absolute_resize: false,
                out_of_process_lifecycle: true,
                replay: false,
            }),
            promotion_readiness: RuntimeTerminalHostPromotionReadinessDashboard::default(),
            session_name: Some("brehon-session-session-abc".to_string()),
            socket_name: None,
            socket_dir: None,
            binary_path: None,
            diagnostics: vec![RuntimeTerminalHostDiagnosticDashboard {
                severity: RuntimeTerminalHostDiagnosticSeverityDashboard::Error,
                code: "terminal_host_session_lost".to_string(),
                message: "1 terminal-host pane is dead after host loss".to_string(),
                action: Some("reset affected panes".to_string()),
            }],
        };

        let summary = runtime_terminal_host_summary(Some(&status));

        assert!(summary.contains("host=headless experimental"));
        assert!(summary.contains("observation=on"));
        assert!(summary.contains("commands=mux"));
        assert!(summary.contains("pane_owner=mux"));
        assert!(summary.contains("agent_factory=mux"));
        assert!(summary.contains("resize=unsupported"));
        assert!(summary.contains("session=brehon-session-session-abc"));
        assert!(summary.contains("diagnostics=error:1"));
    }

    #[test]
    fn test_runtime_registry_preview_prioritizes_active_host_source() {
        let status = RuntimeDaemonDashboardStatus {
            generated_at_ms: 1,
            running: true,
            metrics: RuntimeDaemonDashboardMetrics::default(),
            registry_count: 2,
            registry: RuntimePaneRegistryDashboardSnapshot {
                panes: vec![
                    RuntimePaneDashboardInfo {
                        session_id: "session-1".to_string(),
                        pane_id: "worker-1".to_string(),
                        generation: 1,
                        state: brehon_types::RuntimePaneState::Ready,
                        kind: brehon_types::RuntimePaneKind::Worker,
                        source: Some(brehon_types::RuntimeSource::Mux),
                        title: None,
                        last_output_ms: None,
                        exit_code: None,
                        exit_reason: None,
                        blocked: None,
                    },
                    RuntimePaneDashboardInfo {
                        session_id: "session-1".to_string(),
                        pane_id: "host-preview".to_string(),
                        generation: 1,
                        state: brehon_types::RuntimePaneState::Ready,
                        kind: brehon_types::RuntimePaneKind::Shell,
                        source: Some(brehon_types::RuntimeSource::Headless),
                        title: None,
                        last_output_ms: None,
                        exit_code: None,
                        exit_reason: None,
                        blocked: None,
                    },
                ],
            },
            approvals: RuntimeApprovalDashboardSnapshot::default(),
            sidecar: None,
            terminal_host: Some(RuntimeTerminalHostDashboardStatus {
                kind: brehon_types::RuntimeTerminalHostKind::Headless,
                experimental: true,
                observation_running: true,
                command_routing: RuntimeTerminalHostCommandRoutingDashboard::Mux,
                pane_ownership: brehon_types::RuntimeTerminalHostPaneOwnership::Mux,
                agent_factory: RuntimeTerminalHostAgentFactoryRoutingDashboard::Mux,
                capabilities: None,
                promotion_readiness: RuntimeTerminalHostPromotionReadinessDashboard::default(),
                session_name: Some("brehon-session".to_string()),
                socket_name: None,
                socket_dir: None,
                binary_path: None,
                diagnostics: Vec::new(),
            }),
        };

        let preview = runtime_registry_preview_panes(&status);

        assert_eq!(preview[0].pane_id, "host-preview");
        assert_eq!(preview[1].pane_id, "worker-1");
    }

    #[test]
    fn test_dashboard_agent_status_snapshot_covers_role_glyphs_and_states() {
        use insta::assert_snapshot;

        let now = std::time::Instant::now();
        let snapshot = [
            (
                "worker",
                brehon_mux::PaneKind::Worker,
                Some(brehon_mux::PaneState::Busy {
                    prompt_id: brehon_types::PromptId::new("busy-worker"),
                    generation: brehon_mux::Generation::default(),
                    delivered_at: now,
                    last_activity_at: now,
                }),
                false,
                0,
            ),
            (
                "supervisor",
                brehon_mux::PaneKind::Supervisor,
                None,
                true,
                0,
            ),
            (
                "reviewer",
                brehon_mux::PaneKind::Reviewer,
                Some(brehon_mux::PaneState::Ready { since: now }),
                false,
                0,
            ),
            (
                "director",
                brehon_mux::PaneKind::Director,
                Some(brehon_mux::PaneState::Dead {
                    reason: brehon_mux::DeathReason::SessionDropped,
                    at: now,
                }),
                false,
                0,
            ),
            (
                "shell",
                brehon_mux::PaneKind::Shell,
                Some(brehon_mux::PaneState::Ready { since: now }),
                false,
                0,
            ),
        ]
        .into_iter()
        .map(|(label, kind, pane_state, registered, tick)| {
            let (status_glyph, status_text, status_kind) =
                dashboard_agent_status_for_state(pane_state.as_ref(), registered, tick);
            format!(
                "{label:<10} [{}]@{} [{} {}]@{}",
                crate::theme::role::glyph(&kind),
                color_label(crate::theme::role::color(&kind)),
                status_glyph,
                status_text,
                color_label(
                    crate::theme::status_style(status_kind)
                        .fg
                        .unwrap_or(Color::Reset)
                ),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

        assert_snapshot!("dashboard_factory_status_role_glyphs_and_states", snapshot);
    }

    #[test]
    fn test_dashboard_agent_status_uses_distinct_blocked_glyph() {
        let now = Instant::now();
        let (blocked_glyph, blocked_text, blocked_kind) = dashboard_agent_status_for_state(
            Some(&brehon_mux::PaneState::Blocked {
                info: brehon_types::RuntimePaneBlockInfo {
                    kind: brehon_types::RuntimePaneBlockKind::TerminalPrompt,
                    summary: "terminal prompt".to_string(),
                    command_or_tool: Some("allow cargo test".to_string()),
                    request_id: Some("prompt-1".to_string()),
                    task_id: Some("T-1".to_string()),
                    excerpt: None,
                },
                at: now,
            }),
            true,
            0,
        );
        let (starting_glyph, starting_text, starting_kind) =
            dashboard_agent_status_for_state(None, true, 0);

        assert_eq!(blocked_glyph, "⛔");
        assert_eq!(blocked_text, "blocked");
        assert_eq!(blocked_kind, StatusKind::Warning);
        assert_eq!(starting_glyph, "◐");
        assert_eq!(starting_text, "starting");
        assert_eq!(starting_kind, StatusKind::Info);
        assert_ne!(blocked_glyph, starting_glyph);
    }

    #[test]
    fn test_render_dashboard_embeds_factory_status_and_tasks_titles_in_top_rule() {
        use insta::assert_snapshot;

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_custom_worker_pane(
            "soft-ram-33",
            "codex-ollama-worker",
        ));

        let dashboard = DashboardData {
            agents: vec![AgentInfo {
                name: "soft-ram-33".to_string(),
                role: "worker".to_string(),
                cli: "codex".to_string(),
                session_id: Some("sess-1".to_string()),
                last_seen_at: None,
            }],
            tasks: vec![make_task(
                "T-1",
                "Audit panel usage",
                "in_progress",
                "task",
                None,
            )],
            events: Vec::new(),
            brehon_root: None,
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        let snapshot = (0..14)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert_snapshot!("dashboard_panel_titles_embedded", snapshot);
    }

    #[test]
    fn test_render_dashboard_activity_uses_timestamp_glyph_and_message_columns() {
        use insta::assert_snapshot;

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let mux = Mux::new(24, 80);
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: Vec::new(),
            events: vec![
                EventInfo {
                    timestamp: "00:01".to_string(),
                    description: "worker ready at pane-1".to_string(),
                },
                EventInfo {
                    timestamp: "00:02".to_string(),
                    description: "reset worker pane-2 after provider/runtime failure".to_string(),
                },
                EventInfo {
                    timestamp: "00:03".to_string(),
                    description: "planned launch for reviewer pool".to_string(),
                },
                EventInfo {
                    timestamp: "00:04".to_string(),
                    description: "warning: stale queued prompt".to_string(),
                },
                EventInfo {
                    timestamp: "00:05".to_string(),
                    description: "gateway delivery failed for worker-3".to_string(),
                },
            ],
            brehon_root: None,
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        let snapshot = (11..19)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert_snapshot!("dashboard_activity_columns", snapshot);
    }

    fn render_empty_dashboard_snapshot(rows: std::ops::Range<u16>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 22)).unwrap();
        let mux = Mux::new(24, 80);
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: None,
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState::default();
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                let _ = render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 22),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        rows.map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_render_dashboard_agents_empty_state_card_snapshot() {
        use insta::assert_snapshot;

        let snapshot = render_empty_dashboard_snapshot(1..7);
        assert!(snapshot.contains("⬡ No agents online"));
        assert_snapshot!("dashboard_agents_empty_card", snapshot);
    }

    #[test]
    fn test_render_dashboard_tasks_empty_state_card_snapshot() {
        use insta::assert_snapshot;

        let snapshot = render_empty_dashboard_snapshot(7..13);
        assert!(snapshot.contains("◆ No tasks assigned"));
        assert_snapshot!("dashboard_tasks_empty_card", snapshot);
    }

    #[test]
    fn test_render_dashboard_activity_empty_state_card_snapshot() {
        use insta::assert_snapshot;

        let snapshot = render_empty_dashboard_snapshot(13..21);
        assert!(snapshot.contains("◌ No recent events"));
        assert_snapshot!("dashboard_activity_empty_card", snapshot);
    }

    #[test]
    fn test_render_dashboard_agents_caps_height_and_reports_overflow() {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let mut mux = Mux::new(24, 80);
        let mut agents = Vec::new();

        for idx in 0..12 {
            let name = format!("worker-{idx:02}");
            mux.add_pane(make_custom_worker_pane(&name, "copilot-worker"));
            agents.push(AgentInfo {
                name,
                role: "worker".to_string(),
                cli: "copilot".to_string(),
                session_id: Some(format!("sess-{idx}")),
                last_seen_at: None,
            });
        }

        let dashboard = DashboardData {
            agents,
            tasks: Vec::new(),
            events: Vec::new(),
            brehon_root: None,
        };
        let mut expanded = std::collections::HashSet::new();
        let mut agent_list = DashboardAgentListState {
            scroll: 2,
            max_scroll: 0,
            area: Rect::default(),
        };
        let mut task_list = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    Rect::new(0, 0, 120, 20),
                    &mux,
                    &dashboard,
                    &mut expanded,
                    &mut agent_list,
                    &mut task_list,
                    &[],
                    0,
                );
            })
            .unwrap();

        assert!(agent_list.max_scroll > 0);

        let rows: Vec<String> = (0..terminal.backend().buffer().area.height)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect();
        assert!(!rows.iter().any(|row| row.contains("worker-00")));
        assert!(rows.iter().any(|row| row.contains("worker-02")));

        let footer_row = rows
            .iter()
            .find(|row| row.contains("showing"))
            .expect("dashboard overflow footer");
        assert!(footer_row.contains("of 12"));
    }

    #[test]
    fn test_render_dashboard_tasks_emits_regions_for_initiative_hierarchy() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut initiative = make_task("I-1", "Initiative", "pending", "initiative", None);
        initiative.tokens_used = 12_345;
        let mut epic = make_task("E-1", "Epic", "pending", "epic", Some("I-1"));
        epic.tokens_used = 6_789;
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![
                initiative,
                epic,
                make_task("T-1", "Task", "in_progress", "task", Some("E-1")),
            ],
            events: Vec::new(),
            brehon_root: None,
        };
        let expanded = std::collections::HashSet::from(["I-1".to_string(), "E-1".to_string()]);
        let mut regions = Vec::new();
        let mut state = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                regions = render_dashboard_tasks(
                    frame,
                    Rect::new(0, 0, 120, 18),
                    &dashboard,
                    &expanded,
                    &mut state,
                );
            })
            .unwrap();

        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::EpicToggle("I-1".to_string())));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("E-1".to_string())));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("T-1".to_string())));
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("12,345 tok"));
        assert!(rendered.contains("6,789 tok"));
    }

    #[test]
    fn test_render_dashboard_tasks_uses_arrow_and_role_glyph_styling_snapshot() {
        use insta::assert_snapshot;

        let mut terminal = Terminal::new(TestBackend::new(120, 14)).unwrap();
        let mut expanded = std::collections::HashSet::new();
        expanded.insert("I-1".to_string());

        let mut leaf = make_task(
            "T-1",
            "Expanded child task",
            "in_progress",
            "task",
            Some("I-1"),
        );
        leaf.assignee = Some("worker-1".to_string());

        let mut collapsed_epic = make_task("E-1", "Collapsed epic", "pending", "epic", Some("I-1"));
        collapsed_epic.assignee = Some("supervisor-1".to_string());

        let mut orphan = make_task("T-2", "Reviewer owned task", "review_ready", "task", None);
        orphan.assignee = Some("reviewer-1".to_string());

        let dashboard = DashboardData {
            agents: vec![
                AgentInfo {
                    name: "worker-1".to_string(),
                    role: "worker".to_string(),
                    cli: "codex".to_string(),
                    session_id: Some("sess-worker".to_string()),
                    last_seen_at: None,
                },
                AgentInfo {
                    name: "reviewer-1".to_string(),
                    role: "reviewer".to_string(),
                    cli: "claude".to_string(),
                    session_id: Some("sess-reviewer".to_string()),
                    last_seen_at: None,
                },
            ],
            tasks: vec![
                make_task("I-1", "Initiative", "pending", "initiative", None),
                collapsed_epic,
                leaf,
                orphan,
            ],
            events: Vec::new(),
            brehon_root: None,
        };
        let mut state = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                let _ = render_dashboard_tasks(
                    frame,
                    Rect::new(0, 0, 120, 14),
                    &dashboard,
                    &expanded,
                    &mut state,
                );
            })
            .unwrap();

        let snapshot = (0..10)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert_snapshot!("dashboard_task_tree_glyphs", snapshot);
    }

    #[test]
    fn test_render_dashboard_tasks_scrolls_and_remaps_visible_regions() {
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![
                make_task("E-1", "Epic", "pending", "epic", None),
                make_task("T-1", "Child 1", "pending", "task", Some("E-1")),
                make_task("T-2", "Child 2", "pending", "task", Some("E-1")),
                make_task("T-3", "Child 3", "pending", "task", Some("E-1")),
                make_task("T-4", "Child 4", "pending", "task", Some("E-1")),
                make_task("T-5", "Child 5", "pending", "task", Some("E-1")),
            ],
            events: Vec::new(),
            brehon_root: None,
        };
        let expanded = std::collections::HashSet::from(["E-1".to_string()]);
        let mut regions = Vec::new();
        let mut state = DashboardTaskListState {
            scroll: 2,
            max_scroll: 0,
            area: Rect::default(),
            ..DashboardTaskListState::default()
        };

        terminal
            .draw(|frame| {
                regions = render_dashboard_tasks(
                    frame,
                    Rect::new(0, 0, 120, 7),
                    &dashboard,
                    &expanded,
                    &mut state,
                );
            })
            .unwrap();

        assert!(state.max_scroll > 0);
        assert!(!regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("E-1".to_string())));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("T-2".to_string())));
        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("T-5".to_string())));
    }

    #[test]
    fn test_render_dashboard_tasks_progress_excludes_approved_and_released() {
        let mut terminal = Terminal::new(TestBackend::new(120, 18)).unwrap();
        let mut integrated = make_task("T-int", "Integrated Child", "closed", "task", Some("E-1"));
        integrated.integration_status = Some("integrated".to_string());
        let approved = make_task("T-app", "Approved Child", "approved", "task", Some("E-1"));
        let mut released = make_task("T-rel", "Released Child", "in_review", "task", Some("E-1"));
        released.review_status = Some("released".to_string());
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![
                make_task("E-1", "Epic", "pending", "epic", None),
                integrated,
                approved,
                released,
            ],
            events: Vec::new(),
            brehon_root: None,
        };
        let expanded = std::collections::HashSet::from(["E-1".to_string()]);
        let mut regions = Vec::new();
        let mut state = DashboardTaskListState::default();

        terminal
            .draw(|frame| {
                regions = render_dashboard_tasks(
                    frame,
                    Rect::new(0, 0, 120, 18),
                    &dashboard,
                    &expanded,
                    &mut state,
                );
            })
            .unwrap();

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(regions
            .iter()
            .any(|region| region.target == ClickTarget::TaskDetail("E-1".to_string())));
        assert!(
            content.contains("Epic [1/3]"),
            "expected only integrated child to count toward progress, got: {content}"
        );
        assert!(!content.contains("Epic [2/3]"));
        assert!(!content.contains("Epic [3/3]"));
    }

    #[test]
    fn test_build_task_detail_lines_include_runtime_metadata() {
        let mut task = make_task(
            "T-1",
            "Detail Task",
            "changes_requested",
            "task",
            Some("E-1"),
        );
        task.blockers = Some("Need review follow-up".to_string());
        task.merged_branch = Some("main".to_string());
        task.merged_commit = Some("abc1234".to_string());
        task.closed_by = Some("claude-code".to_string());
        task.closed_at = Some("2026-04-06T02:00:00Z".to_string());
        task.tokens_used = 12_345;

        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![
                make_task("E-1", "Epic", "in_progress", "epic", None),
                task.clone(),
                make_task("T-2", "Sibling", "pending", "task", Some("E-1")),
            ],
            events: Vec::new(),
            brehon_root: None,
        };

        let rendered = lines_to_string(&build_task_detail_lines(&task, &dashboard));
        assert!(rendered.contains("T-1"), "should contain task ID");
        assert!(rendered.contains("E-1"), "should contain parent ID");
        assert!(rendered.contains("50%"), "should contain progress");
        assert!(rendered.contains("12,345"), "should contain token usage");
        assert!(rendered.contains("Detailed brief for T-1"));
        assert!(rendered.contains("Current Activity"));
        assert!(rendered.contains("Latest progress note"));
        assert!(rendered.contains("Need review follow-up"));
        assert!(rendered.contains("main"), "should contain merged branch");
        assert!(rendered.contains("abc1234"), "should contain merged commit");
    }

    #[test]
    fn test_render_task_detail_dialog_uses_distinct_overlay_frame() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let dashboard = DashboardData {
            agents: Vec::new(),
            tasks: vec![make_task("T-1", "Detail Task", "in_progress", "task", None)],
            events: Vec::new(),
            brehon_root: None,
        };
        let mut state = TaskDetailState::new("T-1");

        terminal
            .draw(|frame| {
                render_task_detail_dialog(frame, Rect::new(0, 0, 120, 30), &dashboard, &mut state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let matte_cell = buffer
            .cell((state.area.x.saturating_sub(1), state.area.y))
            .unwrap();
        assert_eq!(matte_cell.symbol(), " ");
        assert_eq!(matte_cell.bg, crate::theme::chrome::PANEL_MATTE_BG);

        let border_cell = buffer.cell((state.area.x, state.area.y)).unwrap();
        assert_eq!(border_cell.symbol(), "╭");
        assert_eq!(border_cell.fg, crate::theme::chrome::PANEL_BORDER);

        let content_cell = buffer.cell((state.area.x + 3, state.area.y + 3)).unwrap();
        assert_eq!(content_cell.bg, crate::theme::chrome::PANEL_BG);
    }

    #[test]
    fn test_keybind_overlay_shortcut_opens_and_dismisses_modal() {
        let mut input_mode = InputMode::default();
        let help_key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::empty());

        assert!(handle_keybind_overlay_key_event(&help_key, &mut input_mode));
        assert!(matches!(input_mode, InputMode::KeybindOverlay(_)));

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                if let InputMode::KeybindOverlay(ref mut state) = input_mode {
                    render_keybind_overlay(frame, Rect::new(0, 0, 120, 30), state);
                }
            })
            .unwrap();

        let rendered = (0..30)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Keyboard shortcuts"));
        assert!(rendered.contains("Show keyboard help"));
        assert!(rendered.contains("Context"));

        let dismiss_key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty());
        assert!(handle_keybind_overlay_key_event(
            &dismiss_key,
            &mut input_mode
        ));
        assert!(matches!(input_mode, InputMode::Normal));

        let mut cleared = Terminal::new(TestBackend::new(120, 30)).unwrap();
        cleared.draw(|_| {}).unwrap();
        let cleared_render = (0..30)
            .map(|row| buffer_row_string(cleared.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!cleared_render.contains("Keyboard shortcuts"));
    }

    #[test]
    fn test_strip_ansi_codes_basic() {
        let plain = "Hello world";
        assert_eq!(strip_ansi_codes(plain), "Hello world");

        let with_color = "\x1b[32mHello\x1b[0m world";
        assert_eq!(strip_ansi_codes(with_color), "Hello world");

        let with_bold = "\x1b[1mBold\x1b[0m text";
        assert_eq!(strip_ansi_codes(with_bold), "Bold text");
    }

    #[test]
    fn test_strip_ansi_codes_multiline() {
        let text = "\x1b[33mLine 1\nLine 2\x1b[0m\n\x1b[32mLine 3\x1b[0m";
        let result = strip_ansi_codes(text);
        assert_eq!(result, "Line 1\nLine 2\nLine 3");
    }

    #[test]
    fn test_strip_ansi_codes_complex() {
        let text = "\x1b[1;32mSuccess: \x1b[0m\x1b[36m42 tests\x1b[0m passed";
        let result = strip_ansi_codes(text);
        assert_eq!(result, "Success: 42 tests passed");
    }

    #[test]
    fn test_render_structured_pane_empty_buffer() {
        use brehon_mux::{ActivityBuffer, Pane};
        use ratatui::{backend::TestBackend, Terminal};

        let pane = Pane::director("test-director", 24, 80).expect("create pane");
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(
            content.contains("No activity yet"),
            "Empty buffer should show 'No activity yet' placeholder"
        );
    }

    #[test]
    fn test_render_pane_in_area_returns_reset_badge_for_supervisor() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        let mut reset_rect = None;

        terminal
            .draw(|frame| {
                reset_rect = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 100, 8),
                    &mux,
                    "claude-supervisor",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();

        assert!(reset_rect.is_some());
        let top_row = buffer_row_string(terminal.backend().buffer(), 0);
        assert!(top_row.contains("[reset]"));
    }

    #[test]
    fn test_render_pane_in_area_reflects_claude_redraws() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        let mut mux = Mux::new(24, 80);
        let mut pane = make_supervisor_pane("claude-supervisor");
        pane.feed(b"* Vibing... first\r\x1b[2K* Vibing... second")
            .expect("feed redraw");
        mux.add_pane(pane);

        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 100, 8),
                    &mux,
                    "claude-supervisor",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(!rendered.contains("first"));
        assert!(rendered.contains("second"));
    }

    #[test]
    fn test_supervisor_pane_selection_renders_and_copies_display_text() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        let mut mux = Mux::new(24, 80);
        let mut pane = make_supervisor_pane("claude-supervisor");
        pane.feed(b"alpha beta\r\nsecond line")
            .expect("feed supervisor");
        mux.add_pane(pane);

        let selection = SelectionState {
            pane: SelectionPane::Supervisor,
            pane_id: "claude-supervisor".to_string(),
            anchor: PanePos { col: 0, row: 0 },
            extent: PanePos { col: 4, row: 0 },
        };

        assert_eq!(extract_selection_text(&selection, &mux), "alpha");

        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 80, 8),
                    &mux,
                    "claude-supervisor",
                    true,
                    Some(&selection),
                    false,
                );
            })
            .unwrap();

        let cell = terminal.backend().buffer().cell((1, 1)).expect("cell");
        assert_eq!(cell.symbol(), "a");
        assert_eq!(cell.bg, Color::White);
        assert_eq!(cell.fg, Color::Black);
    }

    #[test]
    fn test_render_pane_in_area_hides_reset_badge_for_active_reviewer() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        let mut mux = Mux::new(24, 80);
        let mut pane = make_reviewer_pane("reviewer-1");
        pane.set_review_context(brehon_mux::ReviewContextSnapshot {
            review_id: "REV-1".to_string(),
            task_id: "T-1".to_string(),
            round: 1,
            panel_total: 3,
            panel_done: 0,
            verdict: None,
            score: None,
            findings_summary: None,
            updated_at: std::time::Instant::now(),
        });
        mux.add_pane(pane);
        let mut reset_rect = None;

        terminal
            .draw(|frame| {
                reset_rect = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 100, 8),
                    &mux,
                    "reviewer-1",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();

        assert!(reset_rect.is_none());
        let bottom_row = buffer_row_string(terminal.backend().buffer(), 7);
        assert!(!bottom_row.contains("[reset]"));
    }

    #[test]
    fn test_render_host_owned_pane_in_area_uses_runtime_status() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));
        let status = headless_host_owned_dashboard_status("worker-1");
        let mut reset_rect = None;

        terminal
            .draw(|frame| {
                reset_rect = render_host_owned_pane_in_area(
                    frame,
                    Rect::new(0, 0, 120, 12),
                    &mux,
                    "worker-1",
                    true,
                    Some(&status),
                );
            })
            .unwrap();

        assert!(reset_rect.is_some());
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered.contains("terminal host pane"));
        assert!(rendered.contains("state ready"));
        assert!(rendered.contains("source=headless"));
        assert!(rendered.contains("generation=2"));
        assert!(rendered.contains("pane controls route through runtime commands"));
    }

    #[test]
    fn test_host_owned_layout_uses_full_width_without_supervisor_pane() {
        let areas = layout::calculate_host_owned_layout(
            ratatui::layout::Rect::new(0, 0, 120, 40),
            GroupTab::Dashboard,
            true,
        );

        assert_eq!(areas.group_tab_bar.width, 120);
        assert_eq!(areas.left_content.width, 120);
        assert_eq!(areas.supervisor_area.width, 0);
    }

    #[test]
    fn test_render_pane_in_area_draws_cursor_for_focused_supervisor() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut mux = Mux::new(24, 80);
        let mut pane = make_supervisor_pane("claude-supervisor");
        pane.append_output(b"cursor test").expect("append output");
        let (cursor_col, cursor_row) = pane.cursor_position();
        mux.add_pane(pane);

        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 120, 30),
                    &mux,
                    "claude-supervisor",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let cell = buffer
            .cell((
                1 + cursor_col.saturating_sub(1),
                1 + cursor_row.saturating_sub(1),
            ))
            .expect("cursor cell");
        assert_eq!(cell.bg, crate::theme::agent::color("claude"));
        assert_eq!(cell.fg, Color::Black);
    }

    #[test]
    fn test_render_pane_in_area_preserves_cursor_with_claude_chrome_rows() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut mux = Mux::new(24, 80);
        let mut pane = make_supervisor_pane("claude-supervisor");
        pane.append_output(b"kept line\r\nPress up to edit queued messages\r\ncursor test")
            .expect("append output");
        let (cursor_col, cursor_row) = pane
            .display_cursor_position()
            .expect("display cursor")
            .expect("visible cursor");
        mux.add_pane(pane);

        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 120, 30),
                    &mux,
                    "claude-supervisor",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let cell = buffer
            .cell((
                1 + cursor_col.saturating_sub(1),
                1 + cursor_row.saturating_sub(1),
            ))
            .expect("cursor cell");
        assert_eq!(cell.bg, crate::theme::agent::color("claude"));
        assert_eq!(cell.fg, Color::Black);
    }

    #[test]
    fn test_render_structured_pane_prefers_configured_agent_type_in_title() {
        use brehon_mux::ActivityBuffer;
        use ratatui::{backend::TestBackend, Terminal};

        let pane = make_reviewer_pane_with_agent_type(
            "reviewer-opus",
            brehon_mux::SupervisorCli::Copilot,
            Some("copilot-reviewer-opus"),
        );
        let activity_buffer = ActivityBuffer::new(10);
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();

        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 120, 12);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let header_row = buffer_row_string(terminal.backend().buffer(), 0);
        eprintln!("HEADER ROW: {:?}", header_row);
        assert!(header_row.contains("copilot-reviewer-opus"));
        assert!(!header_row.contains("copilot·reviewer"));
    }

    #[test]
    fn test_render_pane_in_area_prefers_configured_agent_type_in_title() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane_with_agent_type(
            "reviewer-opus",
            brehon_mux::SupervisorCli::Copilot,
            Some("copilot-reviewer-opus"),
        ));

        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 120, 8),
                    &mux,
                    "reviewer-opus",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();

        let header_row = buffer_row_string(terminal.backend().buffer(), 0);
        assert!(header_row.contains("copilot-reviewer-opus"));
        assert!(!header_row.contains("copilot·reviewer"));
    }

    #[test]
    fn test_render_pane_in_area_shows_panesmith_backend_indicator() {
        use ratatui::{backend::TestBackend, Terminal};

        let project_root = tempfile::tempdir().expect("tempdir");
        let mux = Mux::factory(brehon_mux::MuxConfig {
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
        assert_eq!(
            mux.pane_backend_ownership("codex-supervisor"),
            Some(brehon_mux::PaneBackendOwnership::Panesmith)
        );

        let mut terminal = Terminal::new(TestBackend::new(160, 8)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 160, 8),
                    &mux,
                    "codex-supervisor",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();

        let rendered = (0..8)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("panesmith"), "rendered: {rendered}");
    }

    #[test]
    fn test_render_pane_in_area_does_not_add_panesmith_indicator_for_other_backends() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut ghostty_mux = Mux::new(24, 80);
        ghostty_mux.add_pane(make_supervisor_pane("claude-supervisor"));
        let mut terminal = Terminal::new(TestBackend::new(140, 8)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 140, 8),
                    &ghostty_mux,
                    "claude-supervisor",
                    true,
                    None,
                    false,
                );
            })
            .unwrap();
        let rendered = (0..8)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("panesmith"), "rendered: {rendered}");

        let mut gateway_mux = Mux::new(24, 80);
        gateway_mux.add_pane(make_custom_worker_pane("worker-codex", "codex-worker"));
        let mut terminal = Terminal::new(TestBackend::new(140, 8)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 140, 8),
                    &gateway_mux,
                    "worker-codex",
                    true,
                    None,
                    true,
                );
            })
            .unwrap();
        let rendered = (0..8)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("panesmith"), "rendered: {rendered}");
    }

    #[test]
    fn test_apply_entry_chrome_fade_dims_white_text_only() {
        use ratatui::{backend::TestBackend, style::Style, widgets::Paragraph, Terminal};

        let backend = TestBackend::new(6, 2);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("A").style(Style::default().fg(crate::theme::chrome::TEXT)),
                    Rect::new(0, 0, 1, 1),
                );
                frame.render_widget(
                    Paragraph::new("B").style(Style::default().fg(crate::theme::chrome::TEXT_DIM)),
                    Rect::new(2, 0, 1, 1),
                );
                apply_entry_chrome_fade(frame);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let white_cell = buffer.cell((0, 0)).expect("white cell");
        assert!(white_cell.modifier.contains(ratatui::style::Modifier::DIM));

        let dim_cell = buffer.cell((2, 0)).expect("dim token cell");
        assert!(!dim_cell.modifier.contains(ratatui::style::Modifier::DIM));
    }

    #[test]
    fn test_render_pane_in_area_dims_dead_pane_and_shows_error_footer_snapshot() {
        use insta::assert_snapshot;
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
        let mut mux = Mux::new(24, 80);
        let mut pane = make_worker_pane("worker-panic");
        pane.append_output(b"worker panicked at fixture line\r\nretry unavailable")
            .expect("append output");
        mux.add_pane(pane);
        let _ = mux.quarantine(
            "worker-panic",
            brehon_mux::DeathReason::Quarantined("panicked-agent fixture".to_string()),
        );

        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 120, 8),
                    &mux,
                    "worker-panic",
                    false,
                    None,
                    false,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let snapshot = [0u16, 2, 3, 7]
            .into_iter()
            .map(|row| buffer_row_string(buffer, row))
            .collect::<Vec<_>>()
            .join("\n");
        assert_snapshot!("pane_error_dead_snapshot", snapshot);

        let dimmed_cell = buffer.cell((2, 2)).expect("dimmed body cell");
        assert!(dimmed_cell.modifier.contains(ratatui::style::Modifier::DIM));

        let footer_row = buffer_row_string(buffer, 7);
        let footer_col = footer_row
            .find("exited with error: panicked-agent fixture")
            .expect("error footer text present");
        let footer_x = u16::try_from(footer_row[..footer_col].chars().count())
            .expect("footer column fits")
            + 1;
        let footer_cell = buffer.cell((footer_x, 7)).expect("footer cell");
        assert_eq!(footer_cell.fg, crate::theme::status::ERROR);
    }

    #[test]
    fn test_render_pane_in_area_missing_pane_shows_placeholder() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        let mux = Mux::new(24, 80);

        terminal
            .draw(|frame| {
                let reset_rect = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 100, 8),
                    &mux,
                    "old-reviewer",
                    true,
                    None,
                    false,
                );
                assert!(reset_rect.is_none());
            })
            .unwrap();

        let rendered = (0..8)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("old-reviewer"));
        assert!(rendered.contains("missing pane"));
        assert!(rendered.contains("not present in this run"));
    }

    #[test]
    fn test_render_structured_pane_header_footer_snapshot_states() {
        use brehon_mux::{ActivityBuffer, ActivityEntry, ActivityKind};
        use insta::assert_snapshot;
        use ratatui::{backend::TestBackend, Terminal};

        let running_pane = make_reviewer_pane_with_agent_type(
            "reviewer-opus",
            brehon_mux::SupervisorCli::Copilot,
            Some("copilot-reviewer-opus"),
        );
        let idle_pane = make_custom_worker_pane("worker-1", "codex-worker");
        let error_pane = make_supervisor_pane("claude-supervisor");

        let mut running = ActivityBuffer::new(10);
        running.start_tool("tool-1".to_string(), "ReadFile".to_string());

        let idle = ActivityBuffer::new(10);

        let mut error = ActivityBuffer::new(10);
        error.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: std::time::Instant::now(),
            tool_id: Some("tool-2".to_string()),
            tool_name: Some("exec".to_string()),
            status: Some("error".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        let mut terminal = Terminal::new(TestBackend::new(120, 18)).unwrap();
        terminal
            .draw(|frame| {
                render_structured_pane(
                    frame,
                    Rect::new(0, 0, 120, 6),
                    &running_pane,
                    &running,
                    true,
                );
                render_structured_pane(frame, Rect::new(0, 6, 120, 6), &idle_pane, &idle, false);
                render_structured_pane(frame, Rect::new(0, 12, 120, 6), &error_pane, &error, false);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let snapshot = [0u16, 5, 6, 11, 12, 17]
            .into_iter()
            .map(|row| buffer_row_string(buffer, row))
            .collect::<Vec<_>>()
            .join("\n");
        assert_snapshot!("pane_header_footer_states", snapshot);

        let gradient = crate::theme::brand::gradient(
            crate::theme::brand::PRIMARY_RGB,
            crate::theme::brand::SECONDARY_RGB,
            "BREHON",
        );
        for (idx, span) in gradient.spans.iter().enumerate() {
            let cell = buffer.cell((3 + idx as u16, 0)).expect("gradient cell");
            assert_eq!(cell.fg, span.style.fg.expect("gradient fg"));
        }
    }

    #[test]
    fn test_render_pane_in_area_gateway_structured_mode_falls_back_to_terminal_output() {
        use ratatui::{backend::TestBackend, Terminal};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut pane = brehon_mux::Pane::worker(
            "worker-opencode",
            dir.path().to_path_buf(),
            None,
            "supervisor",
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::OpenCode),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("create worker pane");
        pane.append_output(b"[brehon] prompt delivered\r\n")
            .expect("append output");

        let mut mux = Mux::new(24, 80);
        mux.add_pane(pane);

        let mut terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render_pane_in_area(
                    frame,
                    Rect::new(0, 0, 120, 8),
                    &mux,
                    "worker-opencode",
                    true,
                    None,
                    true,
                );
            })
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered.contains("prompt delivered"));
    }

    #[test]
    fn test_render_structured_pane_activity_regions_are_visible_only() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = make_custom_worker_pane("worker-activity", "codex-worker");
        pane.ensure_activity_buffer();
        for idx in 0..20 {
            pane.activity_buffer_mut()
                .expect("activity buffer")
                .push(brehon_mux::ActivityEntry {
                    kind: brehon_mux::ActivityKind::Progress,
                    ingested_at: std::time::Instant::now(),
                    tool_id: None,
                    tool_name: None,
                    status: None,
                    message: Some(format!("progress row {idx}")),
                    output_chunks: None,
                    duration: None,
                });
        }

        let mut mux = Mux::new(24, 80);
        mux.add_pane(pane);
        let expanded = std::collections::HashSet::new();
        let mut regions = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render_pane_in_area_with_activity_regions(
                    frame,
                    Rect::new(0, 0, 60, 8),
                    &mux,
                    "worker-activity",
                    true,
                    None,
                    true,
                    &expanded,
                    None,
                    Some(&mut regions),
                );
            })
            .unwrap();

        assert!(!regions.is_empty(), "activity rows should be clickable");
        assert!(
            regions.len() <= 6,
            "only visible body rows should get click regions, got {}",
            regions.len()
        );
        assert!(regions.iter().all(|region| {
            matches!(
                &region.target,
                ClickTarget::ActivityRow { pane_id, .. } if pane_id == "worker-activity"
            ) && region.rect.y >= 1
                && region.rect.y < 7
        }));
    }

    #[test]
    fn test_render_structured_pane_activity_row_expands_wrapped_output() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = make_custom_worker_pane("worker-output", "codex-worker");
        pane.ensure_activity_buffer();
        let output = (0..8)
            .map(|idx| format!("line-{idx:02} {}", "x".repeat(42)))
            .collect::<Vec<_>>()
            .join("\n");
        {
            let activity = pane.activity_buffer_mut().expect("activity buffer");
            activity.append_output(&output);
            activity.flush_output_buffer();
        }

        let mut mux = Mux::new(24, 80);
        mux.add_pane(pane);

        let mut regions = Vec::new();
        let collapsed = std::collections::HashSet::new();
        let mut terminal = Terminal::new(TestBackend::new(72, 12)).unwrap();
        terminal
            .draw(|frame| {
                let _ = render_pane_in_area_with_activity_regions(
                    frame,
                    Rect::new(0, 0, 72, 12),
                    &mux,
                    "worker-output",
                    true,
                    None,
                    true,
                    &collapsed,
                    None,
                    Some(&mut regions),
                );
            })
            .unwrap();

        let collapsed_rows = (0..12)
            .map(|row| buffer_row_string(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(collapsed_rows.contains("line-07"));
        assert!(!collapsed_rows.contains("line-00"));

        let entry_key = regions
            .iter()
            .find_map(|region| match &region.target {
                ClickTarget::ActivityRow { entry_key, .. } => Some(entry_key.clone()),
                _ => None,
            })
            .expect("activity row region");
        let mut expanded = std::collections::HashSet::new();
        expanded.insert(("worker-output".to_string(), entry_key));

        let mut expanded_terminal = Terminal::new(TestBackend::new(72, 12)).unwrap();
        expanded_terminal
            .draw(|frame| {
                let _ = render_pane_in_area_with_activity_regions(
                    frame,
                    Rect::new(0, 0, 72, 12),
                    &mux,
                    "worker-output",
                    true,
                    None,
                    true,
                    &expanded,
                    None,
                    None,
                );
            })
            .unwrap();

        let expanded_rows = (0..12)
            .map(|row| buffer_row_string(expanded_terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded_rows.contains("line-00"), "{expanded_rows}");
    }

    #[test]
    fn test_render_status_bar_prefers_configured_agent_type() {
        use insta::assert_snapshot;
        use ratatui::{backend::TestBackend, Terminal};
        use std::collections::HashMap;
        use std::time::Instant;

        let mut terminal = Terminal::new(TestBackend::new(220, 1)).unwrap();
        let mut mux = Mux::new(24, 80);
        let last_activity = HashMap::new();
        mux.add_pane(make_reviewer_pane_with_agent_type(
            "reviewer-opus",
            brehon_mux::SupervisorCli::Copilot,
            Some("copilot-reviewer-opus"),
        ));
        mux.focus("reviewer-opus");

        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect::new(0, 0, 220, 1),
                    &mux,
                    &last_activity,
                    Instant::now(),
                );
            })
            .unwrap();

        let status_row = buffer_row_string(terminal.backend().buffer(), 0);
        assert_snapshot!("status_bar_styling", status_row);
        assert!(status_row.contains("(copilot-reviewer-opus)"));

        let col = |needle: &str| {
            let byte_idx = status_row.find(needle).expect("needle in status row");
            u16::try_from(status_row[..byte_idx].chars().count()).expect("column fits in u16")
        };
        let buffer = terminal.backend().buffer();

        let key_cell = buffer.cell((col("C-q"), 0)).expect("key cell");
        assert_eq!(key_cell.fg, crate::theme::brand::PRIMARY);

        let label_cell = buffer.cell((col(":Quit"), 0)).expect("label cell");
        assert_eq!(label_cell.fg, crate::theme::chrome::TEXT_DIM);

        let bullet_cell = buffer
            .cell((col(crate::theme::glyph::BULLET), 0))
            .expect("bullet cell");
        assert_eq!(bullet_cell.symbol(), crate::theme::glyph::BULLET);
        assert_eq!(bullet_cell.fg, crate::theme::chrome::TEXT_MUTED);

        let pane_name_cell = buffer
            .cell((col("reviewer-opus"), 0))
            .expect("pane name cell");
        assert_eq!(pane_name_cell.fg, crate::theme::agent::color("copilot"));
    }

    #[test]
    fn test_supervisor_idle_indicator_threshold_and_clear_on_activity() {
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));

        let mut last_activity = HashMap::new();
        last_activity.insert(
            "claude-supervisor".to_string(),
            now - SUPERVISOR_IDLE_INDICATOR_THRESHOLD,
        );
        assert_eq!(supervisor_idle_duration(&mux, &last_activity, now), None);

        last_activity.insert(
            "claude-supervisor".to_string(),
            now - SUPERVISOR_IDLE_INDICATOR_THRESHOLD - Duration::from_secs(1),
        );
        assert_eq!(
            supervisor_idle_duration(&mux, &last_activity, now).map(|idle_for| idle_for.as_secs()),
            Some(31)
        );

        last_activity.insert("claude-supervisor".to_string(), now);
        assert_eq!(supervisor_idle_duration(&mux, &last_activity, now), None);
    }

    #[test]
    fn test_render_status_bar_shows_supervisor_idle_indicator_left_of_keybinds() {
        use ratatui::{backend::TestBackend, Terminal};
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let mut terminal = Terminal::new(TestBackend::new(220, 1)).unwrap();
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        mux.focus("claude-supervisor");

        let mut last_activity = HashMap::new();
        last_activity.insert(
            "claude-supervisor".to_string(),
            now - SUPERVISOR_IDLE_INDICATOR_THRESHOLD - Duration::from_secs(12),
        );

        terminal
            .draw(|frame| {
                render_status_bar(frame, Rect::new(0, 0, 220, 1), &mux, &last_activity, now);
            })
            .unwrap();

        let status_row = buffer_row_string(terminal.backend().buffer(), 0);
        assert!(status_row.starts_with("◉ supervisor idle 42s"));
        let idle_col = status_row
            .find("◉ supervisor idle 42s")
            .expect("idle indicator present");
        let idle_x =
            u16::try_from(status_row[..idle_col].chars().count()).expect("column fits in u16");
        let idle_cell = terminal
            .backend()
            .buffer()
            .cell((idle_x, 0))
            .expect("idle indicator cell");
        assert_eq!(idle_cell.fg, crate::theme::status::IDLE);
    }

    #[test]
    fn test_handle_pane_click_reset_target_requests_manual_reset() {
        let mut mux = Mux::new(24, 80);
        let mut group_tab = GroupTab::Workers;
        let mut selected_worker = 0usize;
        let mut selected_panel = 0usize;
        let mut selected_member = vec![0usize];
        let worker_ids = vec!["worker-1".to_string()];
        let reviewer_ids: Vec<String> = Vec::new();
        let panels: Vec<ReviewerPanel> = Vec::new();
        let supervisor_id = Some("claude-supervisor".to_string());
        let active_left_id = Some("worker-1".to_string());
        let mut expanded = std::collections::HashSet::new();
        let mut expanded_activity = std::collections::HashSet::new();
        let mut task_detail = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;
        let mut external_terminal_tab_request = None;

        handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 10, 1),
                target: ClickTarget::ResetPane("worker-1".to_string()),
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert_eq!(manual_reset_request.as_deref(), Some("worker-1"));
    }

    #[test]
    fn test_handle_pane_click_epic_toggle_toggles_and_marks_regions_stale() {
        let mut mux = Mux::new(24, 80);
        let mut group_tab = GroupTab::Dashboard;
        let mut selected_worker = 0usize;
        let mut selected_panel = 0usize;
        let mut selected_member = Vec::new();
        let worker_ids = Vec::new();
        let reviewer_ids: Vec<String> = Vec::new();
        let panels: Vec<ReviewerPanel> = Vec::new();
        let supervisor_id = None;
        let active_left_id = None;
        let mut expanded = std::collections::HashSet::new();
        let mut expanded_activity = std::collections::HashSet::new();
        let mut task_detail = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;
        let mut external_terminal_tab_request = None;

        let stale = handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 20, 1),
                target: ClickTarget::EpicToggle("E-1".to_string()),
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(stale);
        assert!(expanded.contains("E-1"));

        let stale = handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 20, 1),
                target: ClickTarget::EpicToggle("E-1".to_string()),
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(stale);
        assert!(!expanded.contains("E-1"));
    }

    #[test]
    fn test_handle_pane_click_activity_row_toggles_and_marks_regions_stale() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_custom_worker_pane("worker-1", "codex-worker"));
        let mut group_tab = GroupTab::Workers;
        let mut selected_worker = 0usize;
        let mut selected_panel = 0usize;
        let mut selected_member = vec![0usize];
        let worker_ids = vec!["worker-1".to_string()];
        let reviewer_ids: Vec<String> = Vec::new();
        let panels: Vec<ReviewerPanel> = Vec::new();
        let supervisor_id = None;
        let active_left_id = Some("worker-1".to_string());
        let mut expanded = std::collections::HashSet::new();
        let mut expanded_activity = std::collections::HashSet::new();
        let mut task_detail = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;
        let mut external_terminal_tab_request = None;
        let target = ClickTarget::ActivityRow {
            pane_id: "worker-1".to_string(),
            entry_key: "entry-1".to_string(),
        };

        let stale = handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 10, 1),
                target: target.clone(),
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(stale, "expanding activity rows changes row geometry");
        assert!(expanded_activity.contains(&("worker-1".to_string(), "entry-1".to_string())));

        let stale = handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 10, 1),
                target,
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert!(stale);
        assert!(!expanded_activity.contains(&("worker-1".to_string(), "entry-1".to_string())));
    }

    #[test]
    fn test_handle_pane_click_runtime_approval_requests_resolution() {
        let mut mux = Mux::new(24, 80);
        let mut group_tab = GroupTab::Dashboard;
        let mut selected_worker = 0usize;
        let mut selected_panel = 0usize;
        let mut selected_member = vec![0usize];
        let worker_ids: Vec<String> = Vec::new();
        let reviewer_ids: Vec<String> = Vec::new();
        let panels: Vec<ReviewerPanel> = Vec::new();
        let supervisor_id = None;
        let active_left_id = None;
        let mut expanded = std::collections::HashSet::new();
        let mut expanded_activity = std::collections::HashSet::new();
        let mut task_detail = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;
        let mut external_terminal_tab_request = None;

        handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 10, 1),
                target: ClickTarget::RuntimeApproval {
                    approval_id: "approval-1".to_string(),
                    session_id: "session-1".to_string(),
                    approved: true,
                },
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert_eq!(
            runtime_approval_request,
            Some(("approval-1".to_string(), "session-1".to_string(), true))
        );
    }

    #[test]
    fn test_handle_pane_click_host_owned_agent_tab_requests_external_tab() {
        let mut mux = Mux::new(24, 80);
        let mut group_tab = GroupTab::Dashboard;
        let mut selected_worker = 0usize;
        let mut selected_panel = 0usize;
        let mut selected_member = vec![0usize];
        let worker_ids = vec!["worker-1".to_string()];
        let reviewer_ids: Vec<String> = Vec::new();
        let panels: Vec<ReviewerPanel> = Vec::new();
        let supervisor_id = None;
        let active_left_id = None;
        let mut expanded = std::collections::HashSet::new();
        let mut expanded_activity = std::collections::HashSet::new();
        let mut task_detail = None;
        let mut external_terminal_tab_request = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;

        handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 20, 1),
                target: ClickTarget::GroupTab(GroupTab::Workers),
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            true,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert_eq!(group_tab, GroupTab::Dashboard);
        assert_eq!(external_terminal_tab_request.as_deref(), Some("Workers"));
    }

    #[test]
    fn test_handle_pane_click_member_tab_prefers_current_panel_when_member_is_duplicated() {
        let mut mux = Mux::new(24, 80);
        let mut group_tab = GroupTab::Reviewers;
        let mut selected_worker = 0usize;
        let mut selected_panel = 1usize;
        let mut selected_member = vec![0usize, 1usize];
        let worker_ids: Vec<String> = Vec::new();
        let reviewer_ids = vec!["dup-reviewer".to_string(), "panel-two-only".to_string()];
        let panels = vec![
            ReviewerPanel {
                name: "primary".to_string(),
                members: vec!["dup-reviewer".to_string(), "panel-one-only".to_string()],
            },
            ReviewerPanel {
                name: "secondary".to_string(),
                members: vec!["panel-two-only".to_string(), "dup-reviewer".to_string()],
            },
        ];
        let supervisor_id = Some("claude-supervisor".to_string());
        let active_left_id = Some("dup-reviewer".to_string());
        let mut expanded = std::collections::HashSet::new();
        let mut expanded_activity = std::collections::HashSet::new();
        let mut task_detail = None;
        let mut manual_reset_request = None;
        let mut runtime_approval_request = None;
        let mut external_terminal_tab_request = None;

        handle_pane_click(
            ratatui::layout::Position::new(5, 0),
            &[ClickRegion {
                rect: Rect::new(0, 0, 10, 1),
                target: ClickTarget::MemberTab("dup-reviewer".to_string()),
            }],
            &mut mux,
            &mut group_tab,
            &mut selected_worker,
            &mut selected_panel,
            &mut selected_member,
            &worker_ids,
            &reviewer_ids,
            &panels,
            &supervisor_id,
            &active_left_id,
            &mut expanded,
            &mut expanded_activity,
            &mut task_detail,
            false,
            &mut external_terminal_tab_request,
            &mut manual_reset_request,
            &mut runtime_approval_request,
        );

        assert_eq!(group_tab, GroupTab::Reviewers);
        assert_eq!(selected_panel, 1);
        assert_eq!(selected_member, vec![0, 1]);
    }

    #[test]
    fn test_render_structured_pane_with_tool_call() {
        use brehon_mux::{ActivityBuffer, ActivityEntry, ActivityKind, Pane};
        use ratatui::{backend::TestBackend, Terminal};

        let pane = Pane::director("test-director", 24, 80).expect("create pane");
        let mut activity_buffer = ActivityBuffer::new(10);

        activity_buffer.start_tool("tool-123".to_string(), "cargo test".to_string());
        activity_buffer.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: std::time::Instant::now(),
            tool_id: Some("tool-123".to_string()),
            tool_name: Some("cargo test".to_string()),
            status: Some("started".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(
            content.contains("cargo test"),
            "Should show tool name 'cargo test'"
        );
        assert!(
            content.contains("⟳"),
            "Started tool should show spinner icon"
        );
    }

    #[test]
    fn test_render_structured_pane_with_completed_tool() {
        use brehon_mux::{ActivityBuffer, ActivityEntry, ActivityKind, Pane};
        use ratatui::{backend::TestBackend, Terminal};

        let pane = Pane::director("test-director", 24, 80).expect("create pane");
        let mut activity_buffer = ActivityBuffer::new(10);

        activity_buffer.start_tool("tool-456".to_string(), "bash".to_string());
        activity_buffer.push(ActivityEntry {
            kind: ActivityKind::ToolCall,
            ingested_at: std::time::Instant::now(),
            tool_id: Some("tool-456".to_string()),
            tool_name: Some("bash".to_string()),
            status: Some("completed".to_string()),
            message: None,
            output_chunks: None,
            duration: None,
        });
        activity_buffer.complete_tool("tool-456");

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("bash"), "Should show tool name 'bash'");
        assert!(
            content.contains("✓"),
            "Completed tool should show checkmark icon"
        );
    }

    #[test]
    fn test_render_structured_pane_with_output() {
        use brehon_mux::{ActivityBuffer, Pane};
        use ratatui::{backend::TestBackend, Terminal};

        let pane = Pane::director("test-director", 24, 80).expect("create pane");
        let mut activity_buffer = ActivityBuffer::new(10);

        activity_buffer.append_output("Build successful\n");
        activity_buffer.append_output("Tests passed\n");
        activity_buffer.flush_output_buffer();

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(
            content.contains("Build successful") || content.contains("Tests passed"),
            "Should show output content"
        );
    }

    #[test]
    fn test_render_structured_pane_with_permission_request() {
        use brehon_mux::{ActivityBuffer, ActivityEntry, ActivityKind, Pane};
        use ratatui::{backend::TestBackend, Terminal};

        let pane = Pane::director("test-director", 24, 80).expect("create pane");
        let mut activity_buffer = ActivityBuffer::new(10);

        activity_buffer.push(ActivityEntry {
            kind: ActivityKind::Permission,
            ingested_at: std::time::Instant::now(),
            tool_id: Some("perm-123".to_string()),
            tool_name: None,
            status: None,
            message: Some("Allow read access?".to_string()),
            output_chunks: None,
            duration: None,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(
            content.contains("Allow read access"),
            "Should show permission message"
        );
        assert!(content.contains("⚠"), "Permission should show warning icon");
    }

    #[test]
    fn test_render_structured_pane_with_progress() {
        use brehon_mux::{ActivityBuffer, ActivityEntry, ActivityKind, Pane};
        use ratatui::{backend::TestBackend, Terminal};

        let pane = Pane::director("test-director", 24, 80).expect("create pane");
        let mut activity_buffer = ActivityBuffer::new(10);

        activity_buffer.push(ActivityEntry {
            kind: ActivityKind::Progress,
            ingested_at: std::time::Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some("50%".to_string()),
            message: Some("Downloading...".to_string()),
            output_chunks: None,
            duration: None,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(
            content.contains("Downloading"),
            "Should show progress message"
        );
        assert!(content.contains("◐"), "Progress should show spinner icon");
    }

    #[test]
    fn test_render_structured_pane_reviewer_mid_round_context() {
        use brehon_mux::{ActivityBuffer, ReviewContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = make_reviewer_pane("reviewer-mid");
        pane.set_review_context(ReviewContextSnapshot {
            review_id: "R-mid".to_string(),
            task_id: "T-mid".to_string(),
            round: 2,
            panel_total: 3,
            panel_done: 1,
            verdict: None,
            score: None,
            findings_summary: None,
            updated_at: std::time::Instant::now(),
        });
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("Review context"));
        assert!(content.contains("review R-mid  task T-mid  round 2"));
        assert!(content.contains("panel 1/3 complete"));
        assert!(content.contains("verdict pending"));
        assert!(content.contains("score pending"));
    }

    #[test]
    fn test_render_structured_pane_reviewer_completed_context_with_truncated_findings() {
        use brehon_mux::{ActivityBuffer, ReviewContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = make_reviewer_pane("reviewer-done");
        let long_summary = format!("{} ENDMARK", "x".repeat(220));
        pane.set_review_context(ReviewContextSnapshot {
            review_id: "R-done".to_string(),
            task_id: "T-done".to_string(),
            round: 1,
            panel_total: 3,
            panel_done: 3,
            verdict: Some("approve".to_string()),
            score: Some(9),
            findings_summary: Some(long_summary),
            updated_at: std::time::Instant::now(),
        });
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("verdict approve"));
        assert!(content.contains("score 9"));
        assert!(content.contains("findings"));
        assert!(
            !content.contains("ENDMARK"),
            "tail marker from untruncated summary should not appear"
        );
    }

    #[test]
    fn test_truncate_with_ellipsis_caps_and_suffixes() {
        let input = format!("{}ENDMARK", "x".repeat(240));
        let truncated = truncate_with_ellipsis(&input, REVIEW_FINDINGS_SUMMARY_MAX_CHARS);

        assert_eq!(truncated.chars().count(), REVIEW_FINDINGS_SUMMARY_MAX_CHARS);
        assert!(truncated.ends_with("..."));
        assert!(!truncated.contains("ENDMARK"));
    }

    #[test]
    fn test_render_structured_pane_non_reviewer_never_shows_review_context() {
        use brehon_mux::{ActivityBuffer, Pane, ReviewContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = Pane::director("test-director", 24, 80).expect("create pane");
        pane.set_review_context(ReviewContextSnapshot {
            review_id: "R-hidden".to_string(),
            task_id: "T-hidden".to_string(),
            round: 1,
            panel_total: 1,
            panel_done: 0,
            verdict: None,
            score: None,
            findings_summary: Some("should not render".to_string()),
            updated_at: std::time::Instant::now(),
        });
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(!content.contains("Review context"));
        assert!(!content.contains("R-hidden"));
    }

    #[test]
    fn test_render_structured_pane_with_full_task_context() {
        use brehon_mux::{ActivityBuffer, Pane, TaskBlockedReason, TaskContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = Pane::director("test-director", 24, 100).expect("create pane");
        pane.set_task_context(TaskContextSnapshot {
            task_id: "T-full".to_string(),
            title: "Render full context".to_string(),
            status: TaskStatus::Blocked,
            completion_mode: Some("merge".to_string()),
            merge_target: Some("epic/full-context".to_string()),
            parent_id: Some("E-full".to_string()),
            epic_branch: Some("epic/full-context".to_string()),
            epic_worktree: Some(std::path::PathBuf::from("/tmp/worktrees/epic-full-context")),
            blocked_reason: Some(TaskBlockedReason {
                blocker_task_id: Some("T-base".to_string()),
                summary: Some("Waiting on foundational refactor".to_string()),
            }),
            updated_at: std::time::Instant::now(),
        });
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 100, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("T-full"));
        assert!(content.contains("Render full context"));
        assert!(content.contains("blocked"));
        assert!(content.contains("mode:merge"));
        assert!(content.contains("epic/full-context"));
        assert!(content.contains("/tmp/worktrees/epic-full-context"));
        assert!(content.contains("T-base: Waiting on foundational refactor"));
    }

    #[test]
    fn test_render_structured_pane_with_missing_task_context_fields() {
        use brehon_mux::{ActivityBuffer, Pane, TaskContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = Pane::director("test-director", 24, 100).expect("create pane");
        pane.set_task_context(TaskContextSnapshot {
            task_id: "T-missing".to_string(),
            title: "Missing fields".to_string(),
            status: TaskStatus::Assigned,
            completion_mode: None,
            merge_target: None,
            parent_id: None,
            epic_branch: None,
            epic_worktree: None,
            blocked_reason: None,
            updated_at: std::time::Instant::now(),
        });
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 100, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("T-missing"));
        assert!(content.contains("mode:unknown"));
        assert!(content.contains("merge unknown"));
        assert!(content.contains("epic:unknown"));
        assert!(content.contains("worktree unknown"));
        assert!(content.contains("blocked unknown"));
    }

    #[test]
    fn test_render_structured_pane_blocked_with_structured_reason() {
        use brehon_mux::{ActivityBuffer, Pane, TaskBlockedReason, TaskContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = Pane::director("test-director", 24, 100).expect("create pane");
        pane.set_task_context(TaskContextSnapshot {
            task_id: "T-blocked".to_string(),
            title: "Blocked task".to_string(),
            status: TaskStatus::Blocked,
            completion_mode: Some("merge".to_string()),
            merge_target: Some("main".to_string()),
            parent_id: None,
            epic_branch: None,
            epic_worktree: None,
            blocked_reason: Some(TaskBlockedReason {
                blocker_task_id: Some("T-dependency".to_string()),
                summary: Some("Waiting on dependency completion".to_string()),
            }),
            updated_at: std::time::Instant::now(),
        });
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 100, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("T-dependency: Waiting on dependency completion"));
    }

    #[test]
    fn test_render_structured_pane_feature_epic_context() {
        use brehon_mux::{ActivityBuffer, Pane, TaskContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = Pane::director("test-director", 24, 100).expect("create pane");
        pane.set_task_context(TaskContextSnapshot {
            task_id: "T-epic-sub".to_string(),
            title: "Feature epic subtask".to_string(),
            status: TaskStatus::InProgress,
            completion_mode: Some("merge".to_string()),
            merge_target: Some("epic/feature-ui".to_string()),
            parent_id: Some("E-feature-ui".to_string()),
            epic_branch: Some("epic/feature-ui".to_string()),
            epic_worktree: Some(std::path::PathBuf::from("/tmp/worktrees/epic-feature-ui")),
            blocked_reason: None,
            updated_at: std::time::Instant::now(),
        });
        let activity_buffer = ActivityBuffer::new(10);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 100, 24);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("epic/feature-ui"));
        assert!(content.contains("/tmp/worktrees/epic-feature-ui"));
    }

    #[test]
    fn test_render_structured_pane_keeps_task_context_visible_when_body_overflows() {
        use brehon_mux::{ActivityBuffer, Pane, TaskContextSnapshot};
        use ratatui::{backend::TestBackend, Terminal};

        let mut pane = Pane::director("test-director", 12, 100).expect("create pane");
        pane.set_task_context(TaskContextSnapshot {
            task_id: "T-sticky".to_string(),
            title: "Sticky task context".to_string(),
            status: TaskStatus::InProgress,
            completion_mode: Some("merge".to_string()),
            merge_target: Some("epic/sticky".to_string()),
            parent_id: Some("E-sticky".to_string()),
            epic_branch: Some("epic/sticky".to_string()),
            epic_worktree: Some(std::path::PathBuf::from("/tmp/worktrees/sticky")),
            blocked_reason: None,
            updated_at: std::time::Instant::now(),
        });

        let mut activity_buffer = ActivityBuffer::new(50);
        for idx in 0..20 {
            activity_buffer.append_output(&format!("body line {idx}\n"));
        }
        activity_buffer.flush_output_buffer();

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 100, 12);
                render_structured_pane(frame, area, &pane, &activity_buffer, true);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(content.contains("T-sticky"));
        assert!(content.contains("Sticky task context"));
        assert!(content.contains("body line 19"));
    }

    #[test]
    fn test_is_quit_key_accepts_ctrl_q_and_ctrl_backslash() {
        let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let ctrl_shift_q = KeyEvent::new(
            KeyCode::Char('Q'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let ctrl_backslash = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        let raw_ctrl_q = KeyEvent::new(KeyCode::Char('\u{11}'), KeyModifiers::empty());
        let raw_ctrl_q_with_modifier =
            KeyEvent::new(KeyCode::Char('\u{11}'), KeyModifiers::CONTROL);
        let raw_ctrl_backslash = KeyEvent::new(KeyCode::Char('\u{1c}'), KeyModifiers::empty());

        assert!(is_quit_key(&ctrl_q));
        assert!(is_quit_key(&ctrl_shift_q));
        assert!(is_quit_key(&ctrl_backslash));
        assert!(is_quit_key(&raw_ctrl_q));
        assert!(is_quit_key(&raw_ctrl_q_with_modifier));
        assert!(is_quit_key(&raw_ctrl_backslash));
    }

    #[test]
    fn test_is_quit_key_rejects_plain_q() {
        let plain_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(!is_quit_key(&plain_q));
        assert!(!is_quit_key(&ctrl_c));
    }

    #[test]
    fn test_should_handle_key_event_ignores_release() {
        let press = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        let repeat = KeyEvent {
            kind: KeyEventKind::Repeat,
            ..KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty())
        };
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty())
        };

        assert!(should_handle_key_event(&press));
        assert!(should_handle_key_event(&repeat));
        assert!(!should_handle_key_event(&release));
    }

    #[test]
    fn test_focused_supervisor_captures_keyboard() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        mux.focus("claude-supervisor");

        assert!(focused_supervisor_captures_keyboard(&mux, None));
    }

    #[test]
    fn test_focused_worker_does_not_capture_supervisor_keyboard_policy() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_custom_worker_pane("worker-1", "codex-worker"));
        mux.focus("worker-1");

        assert!(!focused_supervisor_captures_keyboard(&mux, None));
    }

    #[test]
    fn test_task_detail_overrides_supervisor_keyboard_capture() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));
        mux.focus("claude-supervisor");
        let detail = TaskDetailState::new("T-123".to_string());

        assert!(!focused_supervisor_captures_keyboard(&mux, Some(&detail)));
    }

    #[test]
    fn test_is_worker_context_reset_candidate_matches_codex_context_error() {
        let mut mux = Mux::new(24, 80);
        let pane = brehon_mux::Pane::worker(
            "worker-1",
            std::path::PathBuf::from("/tmp"),
            None,
            "codex-ollama-worker",
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("create worker pane");
        mux.add_pane(pane);

        let entry = brehon_mux::ActivityEntry {
            kind: brehon_mux::ActivityKind::Progress,
            ingested_at: std::time::Instant::now(),
            tool_id: None,
            tool_name: None,
            status: None,
            message: Some(
                "Codex error: The prompt is too long: 203272, model maximum context length: 202752"
                    .to_string(),
            ),
            output_chunks: None,
            duration: None,
        };

        assert!(is_worker_context_reset_candidate(&mux, "worker-1", &entry));
    }

    #[test]
    fn test_is_worker_context_reset_candidate_matches_codex_stream_disconnect_error() {
        let mut mux = Mux::new(24, 80);
        let pane = brehon_mux::Pane::worker(
            "worker-1",
            std::path::PathBuf::from("/tmp"),
            None,
            "codex-worker-5-3",
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("create worker pane");
        mux.add_pane(pane);

        let entry = brehon_mux::ActivityEntry {
            kind: brehon_mux::ActivityKind::Progress,
            ingested_at: std::time::Instant::now(),
            tool_id: None,
            tool_name: None,
            status: None,
            message: Some(
                "Codex error: stream disconnected before completion: An error occurred while processing your request. You can retry your request, or contact us through our help center."
                    .to_string(),
            ),
            output_chunks: None,
            duration: None,
        };

        assert!(is_worker_context_reset_candidate(&mux, "worker-1", &entry));
    }

    #[test]
    fn test_is_worker_context_reset_candidate_uses_worker_capabilities_not_provider_name() {
        fn assert_candidate(pane: brehon_mux::Pane, message: &str, expected: bool) {
            let pane_id = pane.id().to_string();
            let mut mux = Mux::new(24, 80);
            mux.add_pane(pane);
            let entry = brehon_mux::ActivityEntry {
                kind: brehon_mux::ActivityKind::Progress,
                ingested_at: std::time::Instant::now(),
                tool_id: None,
                tool_name: None,
                status: None,
                message: Some(message.to_string()),
                output_chunks: None,
                duration: None,
            };

            assert_eq!(
                is_worker_context_reset_candidate(&mux, &pane_id, &entry),
                expected,
                "unexpected worker context reset classification for {pane_id} and message {message:?}"
            );
        }

        let context_length = "The prompt is too long: 203272, model maximum context length: 202752";
        let stream_disconnect = "stream disconnected before completion: An error occurred while processing your request. You can retry your request, or contact us through our help center.";

        for message in [context_length, stream_disconnect] {
            assert_candidate(
                make_builtin_worker_pane(
                    "worker-gemini",
                    "gemini-worker",
                    brehon_mux::SupervisorCli::Gemini,
                ),
                message,
                true,
            );
            assert_candidate(
                make_builtin_worker_pane(
                    "worker-kimi",
                    "kimi-worker",
                    brehon_mux::SupervisorCli::Kimi,
                ),
                message,
                true,
            );
            assert_candidate(
                make_builtin_worker_pane(
                    "worker-opencode",
                    "opencode-worker",
                    brehon_mux::SupervisorCli::OpenCode,
                ),
                message,
                true,
            );
            assert_candidate(
                make_custom_worker_pane("worker-custom-acp", "native-openai-worker"),
                message,
                true,
            );
            assert_candidate(
                make_builtin_worker_pane(
                    "worker-claude",
                    "claude-worker",
                    brehon_mux::SupervisorCli::Claude,
                ),
                message,
                false,
            );
            assert_candidate(
                make_builtin_worker_pane(
                    "worker-junie",
                    "junie-worker",
                    brehon_mux::SupervisorCli::Junie,
                ),
                message,
                false,
            );
            assert_candidate(
                make_builtin_worker_pane(
                    "worker-agy",
                    "agy-worker",
                    brehon_mux::SupervisorCli::Agy,
                ),
                message,
                false,
            );
            let custom_pty = custom_interactive_agent("custom-pty", "cat", &[]);
            assert_candidate(
                make_worker_pane_with_adapter("worker-custom-pty", "custom-pty", &custom_pty),
                message,
                false,
            );
        }
    }

    #[test]
    fn test_supervisor_reset_reason_matches_claude_runtime_crash_dump() {
        let mut mux = Mux::new(24, 80);
        let mut pane = make_supervisor_pane("claude-supervisor");
        pane.append_output(
            br#"<anonymous> (/bunfs/root/src/entrypoints/cli.js:577:98876)
TypeError: Cannot read properties of undefined
T===K.execq():if(!K)return_;let S=Number(K[1]);"#,
        )
        .expect("append crash output");
        mux.add_pane(pane);

        assert_eq!(
            supervisor_reset_reason(&mux, "claude-supervisor"),
            Some("runtime crash")
        );
    }

    #[test]
    fn test_build_supervisor_reset_startup_prompt_mentions_recovery() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));

        let prompt = build_supervisor_reset_startup_prompt(&mux, "claude-supervisor", false)
            .expect("startup prompt");
        assert!(prompt.contains("supervisor session reset"));
        assert!(prompt.contains("task action=ready"));
        assert!(prompt.contains("runtime failure"));
    }

    #[test]
    fn test_build_supervisor_reset_startup_prompt_headless_skips_bootstrap() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_supervisor_pane("claude-supervisor"));

        let prompt = build_supervisor_reset_startup_prompt(&mux, "claude-supervisor", true)
            .expect("startup prompt");
        assert!(prompt.contains("unattended headless run"));
        assert!(!prompt.contains("action=session_start name="));
        assert!(prompt.contains("task action=conflicts"));
        assert!(prompt.contains("task action=list task_type=epic"));
        assert!(prompt.contains("Do not wait for operator confirmation"));
    }

    #[test]
    fn test_build_reviewer_reset_startup_prompt_forbids_mcp_polling() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_reviewer_pane_with_agent_type(
            "claude-reviewer",
            brehon_mux::SupervisorCli::Claude,
            None,
        ));

        let prompt =
            build_reviewer_reset_startup_prompt(&mux, "claude-reviewer").expect("startup prompt");
        assert!(prompt.contains("Do not narrate MCP bootstrap/tool calls"));
        assert!(prompt.contains("instead of polling, sleeping, or running shell commands"));
        assert!(prompt.contains("Do NOT proactively discover, reconnect, or call Brehon MCP tools"));
        assert!(!prompt.contains("Call these silently, without narrating each step"));
        assert!(!prompt.contains("action=session_start name="));
        assert!(!prompt.contains(" ; mcp__brehon__agent action=whoami"));
    }

    #[test]
    fn test_build_worker_context_reset_startup_prompt_mentions_existing_task() {
        let mut mux = Mux::new(24, 80);
        let mut pane = brehon_mux::Pane::worker(
            "worker-1",
            std::path::PathBuf::from("/tmp"),
            None,
            "codex-ollama-worker",
            &brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Codex),
            None,
            None,
            24,
            80,
            None,
            None,
            None,
        )
        .expect("create worker pane");
        pane.set_task_context(brehon_mux::TaskContextSnapshot {
            task_id: "T-123".to_string(),
            title: "Example task".to_string(),
            status: TaskStatus::InProgress,
            completion_mode: None,
            merge_target: None,
            parent_id: None,
            epic_branch: None,
            epic_worktree: None,
            blocked_reason: None,
            updated_at: std::time::Instant::now(),
        });
        mux.add_pane(pane);

        let prompt =
            build_worker_context_reset_startup_prompt(&mux, "worker-1").expect("startup prompt");
        assert!(prompt.contains("T-123"));
        assert!(prompt.contains("Example task"));
        assert!(prompt.contains("provider/runtime failure"));
        assert!(prompt.contains("call `mcp__brehon__task action=mine` at most once"));
        assert!(!prompt.contains("action=session_start name="));
        assert!(!prompt.contains("action=whoami"));
    }

    #[test]
    fn test_build_worker_recycle_startup_prompt_forbids_mcp_polling() {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_worker_pane("worker-1"));

        let prompt = build_worker_recycle_startup_prompt(&mux, "worker-1").expect("startup prompt");
        assert!(prompt.contains("Do NOT proactively discover, reconnect, or call Brehon MCP tools"));
        assert!(prompt.contains("call `mcp__brehon__task action=mine` at most once"));
        assert!(prompt.contains("emit at most one short readiness line"));
        assert!(!prompt.contains("action=session_start name="));
        assert!(!prompt.contains("action=whoami"));
    }

    // NOTE: The three tests that previously lived here asserted the removed
    // PromptDeliveryAttempt::Deferred / AsyncGatewayPromptDispatch::Deferred
    // variants and poked Pane::set_tool_executing (now correctly crate-private
    // to brehon-mux). Under the MUX_REDESIGN state machine a busy pane does not
    // "defer" — it receives the prompt and returns Queued { prompt_id, ahead_of },
    // which is covered by the harness tests in brehon-mux/src/mux_tests/.

    #[test]
    fn test_record_prompt_retry_deferral_marks_prompt_not_due_without_incrementing_attempts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prompt_path = dir.path().join("queued.prompt");
        std::fs::write(&prompt_path, "prompt").expect("write prompt");

        let next_retry_at = record_prompt_retry_deferral(
            &prompt_path,
            Duration::from_secs(30),
            "transport deferred queued prompt delivery",
        );

        assert!(prompt_retry_not_due(&prompt_path));
        assert_eq!(read_prompt_retry_attempts(&prompt_path), 0);
        assert!(next_retry_at > chrono::Utc::now());
    }

    #[test]
    fn test_force_prompt_retry_due_preserves_retry_metadata_but_makes_prompt_immediately_eligible()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let prompt_path = dir.path().join("queued.prompt");
        std::fs::write(&prompt_path, "prompt").expect("write prompt");

        record_prompt_retry_deferral(
            &prompt_path,
            Duration::from_secs(30),
            "transport deferred queued prompt delivery",
        );
        assert!(prompt_retry_not_due(&prompt_path));

        assert!(force_prompt_retry_due(&prompt_path));
        assert!(
            !prompt_retry_not_due(&prompt_path),
            "forced retry should make the prompt immediately eligible"
        );

        let meta = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(prompt_retry_meta_path(&prompt_path))
                .expect("retry metadata should exist"),
        )
        .expect("retry metadata should parse");
        assert_eq!(meta["deferrals"], 1);
        assert_eq!(meta["attempts"], 0);
        assert_eq!(
            meta["last_deferred_reason"].as_str(),
            Some("transport deferred queued prompt delivery")
        );
    }

    #[test]
    fn test_force_prompt_retry_due_rejects_non_object_metadata_without_clobbering_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prompt_path = dir.path().join("queued.prompt");
        let meta_path = prompt_retry_meta_path(&prompt_path);
        std::fs::write(&prompt_path, "prompt").expect("write prompt");
        std::fs::write(&meta_path, "\"not an object\"").expect("write retry metadata");

        assert!(
            !force_prompt_retry_due(&prompt_path),
            "forcing retry due should fail for non-object metadata"
        );
        assert_eq!(
            std::fs::read_to_string(&meta_path).expect("retry metadata should remain readable"),
            "\"not an object\""
        );
    }
}
