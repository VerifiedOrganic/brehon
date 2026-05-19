//! Dashboard rendering: agents, tasks tree, activity feed, and task file I/O.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde::Deserialize;
use unicode_width::UnicodeWidthStr;

use crate::components::Panel;
use ratatui::Frame;

use crate::theme::chrome::{TEXT_DIM, TEXT_MUTED};
use crate::theme::{status_style, StatusKind};
use brehon_mux::{AgentAdapter, Mux, Pane, PaneKind, PaneState};
use brehon_types::task::normalize_task_status;
use brehon_types::{
    RuntimeCommand, RuntimeCommandKind, RuntimePaneKind, RuntimePaneState, RuntimeSource,
    RuntimeTerminalHostKind, RuntimeTerminalHostPaneOwnership, TerminalHostCapabilities,
};

use super::helpers::*;
use super::rendering::truncate_to;
use super::task_detail::{compute_display_status, task_dashboard_hint, task_status_kind};
use super::task_scope_summary::{task_counts_toward_completion, task_scope_summary_line};
use super::types::*;

const RUNTIME_DAEMON_HEARTBEAT_STALE_AFTER_MS: u64 = 15_000;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RuntimeDaemonDashboardStatus {
    #[serde(default)]
    pub generated_at_ms: u64,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub metrics: RuntimeDaemonDashboardMetrics,
    #[serde(default)]
    pub registry_count: usize,
    #[serde(default)]
    pub registry: RuntimePaneRegistryDashboardSnapshot,
    #[serde(default)]
    pub approvals: RuntimeApprovalDashboardSnapshot,
    #[serde(default)]
    pub sidecar: Option<RuntimeSidecarDashboardStatus>,
    #[serde(default)]
    pub terminal_host: Option<RuntimeTerminalHostDashboardStatus>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RuntimeDaemonDashboardMetrics {
    #[serde(default)]
    pub published_events: u64,
    #[serde(default)]
    pub routed_commands: u64,
    #[serde(default)]
    pub rejected_commands: u64,
    #[serde(default)]
    pub deferred_commands: u64,
    #[serde(default)]
    pub pending_approvals: usize,
    #[serde(default)]
    pub audit_write_errors: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RuntimeApprovalDashboardSnapshot {
    #[serde(default)]
    pub approvals: Vec<RuntimePendingApprovalDashboardInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RuntimePaneRegistryDashboardSnapshot {
    #[serde(default)]
    pub panes: Vec<RuntimePaneDashboardInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RuntimePaneDashboardInfo {
    pub session_id: String,
    pub pane_id: String,
    #[serde(default)]
    pub generation: u64,
    pub state: RuntimePaneState,
    pub kind: RuntimePaneKind,
    #[serde(default)]
    pub source: Option<RuntimeSource>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub last_output_ms: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub exit_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RuntimePendingApprovalDashboardInfo {
    pub approval_id: String,
    pub reason: String,
    pub command: RuntimeCommand,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RuntimeTerminalHostDashboardStatus {
    pub kind: RuntimeTerminalHostKind,
    #[serde(default)]
    pub experimental: bool,
    #[serde(default)]
    pub observation_running: bool,
    #[serde(default)]
    pub command_routing: RuntimeTerminalHostCommandRoutingDashboard,
    #[serde(default)]
    pub pane_ownership: RuntimeTerminalHostPaneOwnership,
    #[serde(default)]
    pub agent_factory: RuntimeTerminalHostAgentFactoryRoutingDashboard,
    #[serde(default)]
    pub capabilities: Option<TerminalHostCapabilities>,
    #[serde(default)]
    pub promotion_readiness: RuntimeTerminalHostPromotionReadinessDashboard,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub socket_name: Option<String>,
    #[serde(default)]
    pub socket_dir: Option<String>,
    #[serde(default)]
    pub binary_path: Option<String>,
    #[serde(default)]
    pub diagnostics: Vec<RuntimeTerminalHostDiagnosticDashboard>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RuntimeTerminalHostDiagnosticDashboard {
    pub severity: RuntimeTerminalHostDiagnosticSeverityDashboard,
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeTerminalHostDiagnosticSeverityDashboard {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RuntimeTerminalHostPromotionReadinessDashboard {
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeTerminalHostCommandRoutingDashboard {
    #[default]
    Mux,
    TerminalHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeTerminalHostAgentFactoryRoutingDashboard {
    #[default]
    Mux,
    TerminalHost,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct RuntimeSidecarDashboardStatus {
    pub detection_running: bool,
    pub workflow_running: bool,
}

fn centered_text(text: &str, width: u16) -> String {
    let width = width as usize;
    let text_width = text.width();
    if text_width >= width {
        return truncate_to(text, width);
    }
    let left_pad = (width - text_width) / 2;
    format!("{}{}", " ".repeat(left_pad), text)
}

fn render_empty_state_card(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    headline: &str,
    hint: &str,
    detail: &str,
) {
    if area.width < 12 || area.height < 3 {
        return;
    }

    let content_width = headline.width().max(hint.width()).max(detail.width());
    let card_width = (content_width + 6).min(area.width.saturating_sub(2) as usize) as u16;
    let card_height = 5u16.min(area.height);
    let card_x = area.x + area.width.saturating_sub(card_width) / 2;
    let card_y = area.y + area.height.saturating_sub(card_height) / 2;
    let card_area = Rect::new(card_x, card_y, card_width.max(10), card_height);
    let inner = Panel::new(title)
        .accent(DASH_ACCENT)
        .border(DASH_SECTION_BORDER)
        .render(frame, card_area);

    let lines = match inner.height {
        0 => Vec::new(),
        1 => vec![Line::from(Span::styled(
            centered_text(headline, inner.width),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))],
        2 => vec![
            Line::from(Span::styled(
                centered_text(headline, inner.width),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                centered_text(hint, inner.width),
                Style::default().fg(DASH_ACCENT),
            )),
        ],
        _ => vec![
            Line::from(Span::styled(
                centered_text(headline, inner.width),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                centered_text(hint, inner.width),
                Style::default().fg(DASH_ACCENT),
            )),
            Line::from(Span::styled(
                centered_text(detail, inner.width),
                Style::default().fg(TEXT_MUTED),
            )),
        ],
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(crate) fn render_dashboard(
    frame: &mut Frame,
    area: Rect,
    mux: &Mux,
    dashboard: &DashboardData,
    expanded_epics: &mut std::collections::HashSet<String>,
    agent_list: &mut DashboardAgentListState,
    task_list: &mut DashboardTaskListState,
    recent_runtime_commands: &[RuntimeCommandActivity],
    tick: usize,
) -> Vec<ClickRegion> {
    let inner = Panel::new("")
        .border(DASH_SECTION_BORDER)
        .render(frame, area);

    if inner.height < 3 || inner.width < 10 {
        return Vec::new();
    }

    let runtime_status = dashboard
        .brehon_root
        .as_deref()
        .and_then(read_runtime_daemon_dashboard_status);
    let runtime_height = if runtime_status.is_some() && inner.height >= 18 {
        6
    } else {
        0
    };
    let runtime_agent_count = runtime_status
        .as_ref()
        .map(runtime_dashboard_agent_count)
        .unwrap_or_default();
    let desired_agents_height = mux.panes().count().max(runtime_agent_count).max(1) as u16 + 4;
    let max_agents_height = inner
        .height
        .saturating_sub(13 + runtime_height)
        .max(6)
        .min(12);
    let agents_height = desired_agents_height.min(max_agents_height).max(6);
    let mut regions = Vec::new();

    sync_dashboard_container_expansion(dashboard, expanded_epics, task_list);

    if runtime_height > 0 {
        let Some(runtime_status_ref) = runtime_status.as_ref() else {
            return regions;
        };
        let sections = Layout::vertical([
            Constraint::Length(agents_height),
            Constraint::Length(runtime_height),
            Constraint::Min(5),
            Constraint::Length(8),
        ])
        .split(inner);

        render_dashboard_agents(
            frame,
            sections[0],
            mux,
            dashboard,
            agent_list,
            tick,
            Some(runtime_status_ref),
        );
        regions.extend(render_dashboard_runtime(
            frame,
            sections[1],
            runtime_status_ref,
            recent_runtime_commands,
        ));
        regions.extend(render_dashboard_tasks(
            frame,
            sections[2],
            dashboard,
            expanded_epics,
            task_list,
        ));
        render_dashboard_activity(frame, sections[3], dashboard);
        return regions;
    }

    let sections = Layout::vertical([
        Constraint::Length(agents_height),
        Constraint::Min(5),
        Constraint::Length(8),
    ])
    .split(inner);

    render_dashboard_agents(
        frame,
        sections[0],
        mux,
        dashboard,
        agent_list,
        tick,
        runtime_status.as_ref(),
    );
    regions.extend(render_dashboard_tasks(
        frame,
        sections[1],
        dashboard,
        expanded_epics,
        task_list,
    ));
    render_dashboard_activity(frame, sections[2], dashboard);
    regions
}

fn sync_dashboard_container_expansion(
    dashboard: &DashboardData,
    expanded_epics: &mut std::collections::HashSet<String>,
    state: &mut DashboardTaskListState,
) {
    let current_containers: std::collections::HashSet<String> = dashboard
        .tasks
        .iter()
        .filter(|task| task_is_container(task))
        .map(|task| task.id.clone())
        .collect();

    expanded_epics.retain(|id| current_containers.contains(id));
    state
        .known_container_ids
        .retain(|id| current_containers.contains(id));

    // New containers start expanded so fresh work is visible. Once the user
    // collapses one, known_container_ids keeps the render pass from reopening it.
    for id in current_containers {
        if state.known_container_ids.insert(id.clone()) {
            expanded_epics.insert(id);
        }
    }
}

pub(crate) fn render_runtime_view(
    frame: &mut Frame,
    area: Rect,
    brehon_root: Option<&Path>,
    recent_runtime_commands: &[RuntimeCommandActivity],
) -> Vec<ClickRegion> {
    let Some(status) = brehon_root.and_then(read_runtime_daemon_dashboard_status) else {
        let inner = Panel::new("Runtime")
            .border(DASH_SECTION_BORDER)
            .render(frame, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  runtime status unavailable",
                Style::default().fg(TEXT_DIM),
            ))),
            inner,
        );
        return Vec::new();
    };

    if area.height < 12 {
        return render_dashboard_runtime(frame, area, &status, recent_runtime_commands);
    }

    render_expanded_runtime_view(frame, area, &status, recent_runtime_commands)
}

fn render_dashboard_agents(
    frame: &mut Frame,
    area: Rect,
    mux: &Mux,
    dashboard: &DashboardData,
    state: &mut DashboardAgentListState,
    tick: usize,
    runtime_status: Option<&RuntimeDaemonDashboardStatus>,
) {
    let inner = Panel::new("Factory Status")
        .accent(DASH_ACCENT)
        .border(DASH_SECTION_BORDER)
        .render(frame, area);
    state.area = inner;

    if inner.height < 3 || inner.width < 10 {
        state.max_scroll = 0;
        state.scroll = 0;
        return;
    }

    // Include all known agents, not just ones with a session ID, so panes that
    // exist in the mux before the session handshake completes render as
    // "starting" instead of indistinguishable idle entries.
    let known_agents: std::collections::HashMap<String, &AgentInfo> = dashboard
        .agents
        .iter()
        .map(|a| (a.name.clone(), a))
        .collect();

    let left_pad = 2usize;
    let glyph_width = 3usize;
    let agent_width = 14usize;
    let cli_width = 8usize;
    let status_width = 14usize;
    let provider_width = inner
        .width
        .saturating_sub((left_pad + glyph_width + agent_width + cli_width + status_width) as u16)
        as usize;

    let header = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            pad_cell("", glyph_width),
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_cell("Agent", agent_width),
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_cell("CLI", cli_width),
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_cell("Provider", provider_width),
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_cell("Status", status_width),
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        ),
    ]);

    let mut agent_rows: Vec<Line> = Vec::new();

    for pane in mux.panes() {
        let (cli_name, provider_name) = dashboard_agent_identity(pane);
        let kind = pane.kind();
        let cc = crate::theme::agent::color(&cli_name);
        let (status_glyph, status_text, status_kind) =
            dashboard_agent_status(pane, known_agents.get(pane.id()).copied(), tick);
        let role_glyph = crate::theme::role::glyph(kind);
        let role_color = crate::theme::role::color(kind);

        agent_rows.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                pad_cell(role_glyph, glyph_width),
                Style::default().fg(role_color),
            ),
            Span::styled(
                pad_cell(pane.id(), agent_width),
                Style::default().fg(Color::White),
            ),
            Span::styled(pad_cell(&cli_name, cli_width), Style::default().fg(cc)),
            Span::styled(
                pad_cell(&provider_name, provider_width),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                pad_cell(&format!("{status_glyph} {status_text}"), status_width),
                status_style(status_kind),
            ),
        ]));
    }

    if agent_rows.is_empty() {
        if let Some(status) = runtime_status {
            agent_rows.extend(runtime_dashboard_agent_rows(status, provider_width));
        }
    }

    if agent_rows.is_empty() {
        state.max_scroll = 0;
        state.scroll = 0;
        render_empty_state_card(
            frame,
            inner,
            "Start here",
            "⬡ No agents online",
            "start an agent with C-n",
            "spawn a worker, reviewer, or supervisor",
        );
        return;
    }

    let visible_rows = inner.height.saturating_sub(2) as usize;
    let max_scroll = agent_rows.len().saturating_sub(visible_rows) as u16;
    state.max_scroll = max_scroll;
    state.scroll = state.scroll.min(max_scroll);
    let start = state.scroll as usize;
    let end = (start + visible_rows).min(agent_rows.len());

    let mut lines: Vec<Line> = Vec::new();
    lines.push(header);
    lines.extend(agent_rows[start..end].iter().cloned());

    let runtime_agent_total = runtime_status
        .map(runtime_dashboard_agent_count)
        .unwrap_or_default();
    let total = mux.pane_count().max(runtime_agent_total);
    let registered_count = if mux.pane_count() > 0 {
        mux.panes()
            .filter(|p| {
                known_agents
                    .get(p.id())
                    .is_some_and(|info| info.session_id.as_ref().is_some())
            })
            .count()
    } else {
        runtime_status
            .map(runtime_dashboard_ready_agent_count)
            .unwrap_or(0)
    };

    let mut summary_suffix = format!(
        "  |  {} workers  {} reviewers  {} advisors",
        mux.worker_count().max(
            runtime_status
                .map(|status| runtime_dashboard_kind_count(status, RuntimePaneKind::Worker))
                .unwrap_or_default()
        ),
        mux.panes_by_kind(PaneKind::Reviewer).count().max(
            runtime_status
                .map(|status| runtime_dashboard_kind_count(status, RuntimePaneKind::Reviewer))
                .unwrap_or_default()
        ),
        mux.panes_by_kind(PaneKind::Advisor).count().max(
            runtime_status
                .map(|status| runtime_dashboard_kind_count(status, RuntimePaneKind::Advisor))
                .unwrap_or_default()
        )
    );
    if max_scroll > 0 {
        summary_suffix.push_str(&format!(
            "  |  showing {}-{} of {}",
            start + 1,
            end,
            agent_rows.len()
        ));
    }
    if let Some(tokens_used) = dashboard
        .brehon_root
        .as_deref()
        .and_then(read_runtime_token_count)
    {
        summary_suffix.push_str(&format!("  |  tokens {}", format_token_count(tokens_used)));
    }
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {registered_count}/{total} registered"),
            Style::default()
                .fg(if registered_count == total {
                    crate::theme::status::SUCCESS
                } else {
                    crate::theme::status::PENDING
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary_suffix, Style::default().fg(TEXT_DIM)),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn runtime_dashboard_agent_count(status: &RuntimeDaemonDashboardStatus) -> usize {
    status
        .registry
        .panes
        .iter()
        .filter(|pane| runtime_dashboard_pane_is_agent(&pane.kind))
        .count()
}

fn runtime_dashboard_ready_agent_count(status: &RuntimeDaemonDashboardStatus) -> usize {
    status
        .registry
        .panes
        .iter()
        .filter(|pane| {
            runtime_dashboard_pane_is_agent(&pane.kind) && pane.state != RuntimePaneState::Dead
        })
        .count()
}

fn runtime_dashboard_kind_count(
    status: &RuntimeDaemonDashboardStatus,
    kind: RuntimePaneKind,
) -> usize {
    status
        .registry
        .panes
        .iter()
        .filter(|pane| pane.kind == kind)
        .count()
}

fn runtime_dashboard_pane_is_agent(kind: &RuntimePaneKind) -> bool {
    matches!(
        kind,
        RuntimePaneKind::Supervisor
            | RuntimePaneKind::Worker
            | RuntimePaneKind::Reviewer
            | RuntimePaneKind::Advisor
    )
}

fn runtime_dashboard_agent_rows(
    status: &RuntimeDaemonDashboardStatus,
    provider_width: usize,
) -> Vec<Line<'static>> {
    let mut panes = status
        .registry
        .panes
        .iter()
        .filter(|pane| runtime_dashboard_pane_is_agent(&pane.kind))
        .collect::<Vec<_>>();
    panes.sort_by(|left, right| {
        runtime_dashboard_kind_rank(&left.kind)
            .cmp(&runtime_dashboard_kind_rank(&right.kind))
            .then_with(|| left.pane_id.cmp(&right.pane_id))
    });

    let glyph_width = 3usize;
    let agent_width = 14usize;
    let cli_width = 8usize;
    let status_width = 14usize;

    panes
        .into_iter()
        .map(|pane| {
            let (role_glyph, role_color) = runtime_dashboard_role_chrome(&pane.kind);
            let source = pane
                .source
                .as_ref()
                .map(runtime_source_label)
                .unwrap_or_else(|| "unknown".to_string());
            let provider = pane
                .title
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| format!("gen {}", pane.generation));
            let status = runtime_pane_state_label(&pane.state);
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    pad_cell(role_glyph, glyph_width),
                    Style::default().fg(role_color),
                ),
                Span::styled(
                    pad_cell(&pane.pane_id, agent_width),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    pad_cell(&source, cli_width),
                    Style::default().fg(crate::theme::agent::color(&source)),
                ),
                Span::styled(
                    pad_cell(&provider, provider_width),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    pad_cell(status, status_width),
                    runtime_dashboard_state_style(&pane.state),
                ),
            ])
        })
        .collect()
}

fn runtime_dashboard_kind_rank(kind: &RuntimePaneKind) -> usize {
    match kind {
        RuntimePaneKind::Supervisor => 0,
        RuntimePaneKind::Worker => 1,
        RuntimePaneKind::Reviewer => 2,
        RuntimePaneKind::Advisor => 3,
        RuntimePaneKind::Director
        | RuntimePaneKind::Shell
        | RuntimePaneKind::Unknown
        | RuntimePaneKind::Other { .. } => 4,
    }
}

fn runtime_dashboard_role_chrome(kind: &RuntimePaneKind) -> (&'static str, Color) {
    let pane_kind = match kind {
        RuntimePaneKind::Supervisor => PaneKind::Supervisor,
        RuntimePaneKind::Worker => PaneKind::Worker,
        RuntimePaneKind::Reviewer => PaneKind::Reviewer,
        RuntimePaneKind::Advisor => PaneKind::Advisor,
        RuntimePaneKind::Director => PaneKind::Director,
        RuntimePaneKind::Shell | RuntimePaneKind::Unknown | RuntimePaneKind::Other { .. } => {
            PaneKind::Shell
        }
    };
    (
        crate::theme::role::glyph(&pane_kind),
        crate::theme::role::color(&pane_kind),
    )
}

fn runtime_dashboard_state_style(state: &RuntimePaneState) -> Style {
    match state {
        RuntimePaneState::Ready => Style::default().fg(crate::theme::status::SUCCESS),
        RuntimePaneState::Busy => Style::default().fg(crate::theme::status::RUNNING),
        RuntimePaneState::Dead => Style::default().fg(crate::theme::status::ERROR),
        RuntimePaneState::Unknown => Style::default().fg(TEXT_DIM),
    }
}

pub(crate) fn read_runtime_daemon_dashboard_status(
    brehon_root: &std::path::Path,
) -> Option<RuntimeDaemonDashboardStatus> {
    let path = brehon_root
        .join("runtime")
        .join("daemon")
        .join("current.json");
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn read_runtime_token_count(brehon_root: &Path) -> Option<u64> {
    let counters = brehon_types::load_runtime_stability_counters(
        &brehon_types::runtime_stability_counters_path(brehon_root),
    )?;
    Some(counters.tokens_used)
}

pub(crate) fn format_token_count(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted.chars().rev().collect()
}

fn render_dashboard_runtime(
    frame: &mut Frame,
    area: Rect,
    status: &RuntimeDaemonDashboardStatus,
    recent_runtime_commands: &[RuntimeCommandActivity],
) -> Vec<ClickRegion> {
    let mut regions = Vec::new();
    let inner = Panel::new("Runtime")
        .accent(DASH_ACCENT)
        .border(DASH_SECTION_BORDER)
        .render(frame, area);
    if inner.height == 0 || inner.width < 20 {
        return regions;
    }

    let heartbeat_age_ms = runtime_daemon_heartbeat_age_ms(status);
    let heartbeat_stale = runtime_daemon_heartbeat_stale(status);
    let (daemon_glyph, daemon_color, daemon_label) =
        runtime_daemon_status_display(status, heartbeat_stale);
    let sidecar_text = runtime_sidecar_summary(status.sidecar);

    let mut lines = vec![Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{daemon_glyph} "),
            Style::default().fg(daemon_color),
        ),
        Span::styled(
            daemon_label,
            Style::default()
                .fg(daemon_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  age={}ms panes={} events={} cmds={}/{} rejected={} deferred={} approvals={} audit_errors={}  {}",
                heartbeat_age_ms,
                status.registry_count,
                status.metrics.published_events,
                status.metrics.routed_commands,
                status
                    .metrics
                    .routed_commands
                    .saturating_sub(status.metrics.rejected_commands)
                    .saturating_sub(status.metrics.deferred_commands),
                status.metrics.rejected_commands,
                status.metrics.deferred_commands,
                status.metrics.pending_approvals,
                status.metrics.audit_write_errors,
                sidecar_text
            ),
            Style::default().fg(TEXT_DIM),
        ),
    ])];

    if inner.height > lines.len() as u16 {
        lines.push(Line::from(Span::styled(
            format!(
                "  {}  {}",
                runtime_terminal_host_summary_for_status(status),
                runtime_registry_summary(status)
            ),
            Style::default().fg(TEXT_DIM),
        )));
    }

    if status.approvals.approvals.is_empty() {
        let max_registry_rows = inner.height.saturating_sub(lines.len() as u16 + 1) as usize;
        for pane in runtime_registry_preview_panes(status)
            .into_iter()
            .take(max_registry_rows)
        {
            lines.push(Line::from(Span::styled(
                pad_cell(&runtime_pane_summary(pane), inner.width as usize),
                Style::default().fg(Color::White),
            )));
        }
        if let Some(command) = recent_runtime_commands.first() {
            if (lines.len() as u16) < inner.height {
                lines.push(runtime_command_activity_line(command, inner.width));
            }
        }
    }

    if heartbeat_stale && !status.approvals.approvals.is_empty() {
        if (lines.len() as u16) < inner.height {
            lines.push(Line::from(Span::styled(
                "  runtime approvals disabled until heartbeat refreshes",
                Style::default().fg(crate::theme::status::WARNING),
            )));
        }
    } else {
        let approval_start_line = lines.len() as u16;
        let max_approval_rows = inner.height.saturating_sub(approval_start_line) as usize;
        for (idx, approval) in status
            .approvals
            .approvals
            .iter()
            .take(max_approval_rows)
            .enumerate()
        {
            let action_text = "  [approve] [deny]";
            let action_width = action_text.width();
            let main_width = (inner.width as usize).saturating_sub(action_width);
            let main = format!(
                "  {} {} {}",
                short_runtime_id(&approval.approval_id),
                runtime_command_summary(&approval.command),
                approval.reason
            );
            let main = pad_cell(&main, main_width);
            let line_idx = idx as u16;
            lines.push(Line::from(vec![
                Span::styled(main, Style::default().fg(Color::White)),
                Span::styled("  ", Style::default()),
                Span::styled(
                    "[approve]",
                    Style::default()
                        .fg(crate::theme::status::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", Style::default()),
                Span::styled(
                    "[deny]",
                    Style::default()
                        .fg(crate::theme::status::ERROR)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            let approve_x = inner.x + main_width.saturating_add(2).min(inner.width as usize) as u16;
            let deny_x = inner.x
                + main_width
                    .saturating_add(2 + "[approve] ".width())
                    .min(inner.width as usize) as u16;
            let y = inner.y + approval_start_line + line_idx;
            if y < inner.y + inner.height {
                regions.push(ClickRegion {
                    rect: Rect::new(approve_x, y, "[approve]".width() as u16, 1),
                    target: ClickTarget::RuntimeApproval {
                        approval_id: approval.approval_id.clone(),
                        session_id: approval.command.target.session_id.clone(),
                        approved: true,
                    },
                });
                regions.push(ClickRegion {
                    rect: Rect::new(deny_x, y, "[deny]".width() as u16, 1),
                    target: ClickTarget::RuntimeApproval {
                        approval_id: approval.approval_id.clone(),
                        session_id: approval.command.target.session_id.clone(),
                        approved: false,
                    },
                });
            }
        }
    }

    if status.approvals.approvals.is_empty() && (lines.len() as u16) < inner.height {
        lines.push(Line::from(Span::styled(
            "  no pending approvals",
            Style::default().fg(TEXT_DIM),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
    regions
}

fn render_expanded_runtime_view(
    frame: &mut Frame,
    area: Rect,
    status: &RuntimeDaemonDashboardStatus,
    recent_runtime_commands: &[RuntimeCommandActivity],
) -> Vec<ClickRegion> {
    let mut regions = Vec::new();
    let inner = Panel::new("Runtime")
        .accent(DASH_ACCENT)
        .border(DASH_SECTION_BORDER)
        .right_label(runtime_runtime_view_label(status))
        .render(frame, area);
    if inner.height == 0 || inner.width < 20 {
        return regions;
    }

    let heartbeat_age_ms = runtime_daemon_heartbeat_age_ms(status);
    let heartbeat_stale = runtime_daemon_heartbeat_stale(status);
    let (daemon_glyph, daemon_color, daemon_label) =
        runtime_daemon_status_display(status, heartbeat_stale);
    let sidecar_text = runtime_sidecar_summary(status.sidecar);
    let mut lines = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(format!("{daemon_glyph} "), Style::default().fg(daemon_color)),
        Span::styled(
            daemon_label,
            Style::default()
                .fg(daemon_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  age={}ms events={} commands={}/{} rejected={} deferred={} approvals={} audit_errors={} {}",
                heartbeat_age_ms,
                status.metrics.published_events,
                status.metrics.routed_commands,
                status.metrics.routed_commands
                    .saturating_sub(status.metrics.rejected_commands)
                    .saturating_sub(status.metrics.deferred_commands),
                status.metrics.rejected_commands,
                status.metrics.deferred_commands,
                status.metrics.pending_approvals,
                status.metrics.audit_write_errors,
                sidecar_text
            ),
            Style::default().fg(TEXT_DIM),
        ),
    ]));
    if heartbeat_stale {
        lines.push(runtime_line(
            "health=stale heartbeat; command results may lag until refresh",
            inner.width,
            crate::theme::status::WARNING,
        ));
    } else if !status.running {
        lines.push(runtime_line(
            "health=daemon stopped; runtime commands cannot be confirmed",
            inner.width,
            crate::theme::status::ERROR,
        ));
    } else if status.metrics.audit_write_errors > 0 {
        lines.push(runtime_line(
            "health=audit writes failing; command execution may still continue",
            inner.width,
            crate::theme::status::WARNING,
        ));
    }

    runtime_push_heading(&mut lines, "Terminal Host");
    match status.terminal_host.as_ref() {
        Some(host) => {
            lines.push(runtime_line(
                &format!(
                    "mode={} kind={}{} host_panes={} observation={} commands={} pane_owner={} agent_factory={} resize={}",
                    runtime_terminal_host_mode_label(status, host),
                    runtime_terminal_host_kind_label(host.kind),
                    if host.experimental {
                        " experimental"
                    } else {
                        ""
                    },
                    runtime_terminal_host_pane_count(status, host.kind),
                    if host.observation_running {
                        "on"
                    } else {
                        "off"
                    },
                    runtime_terminal_host_command_routing_label(host.command_routing),
                    runtime_terminal_host_pane_ownership_label(host.pane_ownership),
                    runtime_terminal_host_agent_factory_label(host.agent_factory),
                    runtime_terminal_host_resize_label(host)
                ),
                inner.width,
                Color::White,
            ));
            lines.push(runtime_line(
                &runtime_terminal_host_identity_summary(host),
                inner.width,
                TEXT_DIM,
            ));
            if let Some(capabilities) = host.capabilities.as_ref() {
                lines.push(runtime_line(
                    &format!(
                        "capabilities {}",
                        runtime_terminal_host_capabilities_summary(capabilities)
                    ),
                    inner.width,
                    TEXT_DIM,
                ));
            }
            let readiness = runtime_terminal_host_promotion_readiness(host);
            lines.push(runtime_line(
                &format!(
                    "promotion={} blockers={}",
                    if readiness.ready { "ready" } else { "blocked" },
                    readiness.blockers.len()
                ),
                inner.width,
                if readiness.ready {
                    Color::Green
                } else {
                    TEXT_DIM
                },
            ));
            for blocker in &readiness.blockers {
                lines.push(runtime_line(
                    &format!("blocker {blocker}"),
                    inner.width,
                    TEXT_DIM,
                ));
            }
            for diagnostic in &host.diagnostics {
                let color = runtime_terminal_host_diagnostic_color(diagnostic.severity);
                lines.push(runtime_line(
                    &format!(
                        "diagnostic {} {} {}",
                        runtime_terminal_host_diagnostic_severity_label(diagnostic.severity),
                        diagnostic.code,
                        diagnostic.message
                    ),
                    inner.width,
                    color,
                ));
                if let Some(action) = diagnostic.action.as_deref() {
                    lines.push(runtime_line(
                        &format!("diagnostic_action {action}"),
                        inner.width,
                        TEXT_DIM,
                    ));
                }
            }
            if let Some(attach_command) = runtime_terminal_host_attach_command(host) {
                lines.push(runtime_line(
                    &format!("attach={attach_command}"),
                    inner.width,
                    TEXT_DIM,
                ));
            }
        }
        None => lines.push(runtime_line(
            "kind=unknown observation=unknown commands=unknown",
            inner.width,
            TEXT_DIM,
        )),
    }

    runtime_push_heading(&mut lines, "Pane Registry");
    lines.push(runtime_line(
        &runtime_registry_summary(status),
        inner.width,
        Color::White,
    ));

    let approval_reserved = if status.approvals.approvals.is_empty() {
        3
    } else {
        status.approvals.approvals.len().min(4) + 2
    };
    let max_registry_rows = inner
        .height
        .saturating_sub(lines.len() as u16)
        .saturating_sub(approval_reserved as u16)
        .saturating_sub(1) as usize;
    if max_registry_rows > 0 {
        lines.push(runtime_registry_table_header(inner.width));
        for pane in runtime_registry_preview_panes(status)
            .into_iter()
            .take(max_registry_rows.saturating_sub(1))
        {
            lines.push(runtime_registry_table_row(pane, inner.width));
        }
    }

    runtime_push_heading(&mut lines, "Pending Approvals");
    if heartbeat_stale && !status.approvals.approvals.is_empty() {
        lines.push(runtime_line(
            "runtime approvals disabled until heartbeat refreshes",
            inner.width,
            crate::theme::status::WARNING,
        ));
    } else if status.approvals.approvals.is_empty() {
        lines.push(runtime_line("no pending approvals", inner.width, TEXT_DIM));
    } else {
        let max_approval_rows = inner.height.saturating_sub(lines.len() as u16) as usize;
        for approval in status.approvals.approvals.iter().take(max_approval_rows) {
            let action_text = "  [approve] [deny]";
            let action_width = action_text.width();
            let main_width = (inner.width as usize).saturating_sub(action_width);
            let main = format!(
                "{} {} {}",
                short_runtime_id(&approval.approval_id),
                runtime_command_summary(&approval.command),
                approval.reason
            );
            let main = pad_cell(&format!("  {main}"), main_width);
            let y = inner.y + lines.len() as u16;
            lines.push(Line::from(vec![
                Span::styled(main, Style::default().fg(Color::White)),
                Span::styled("  ", Style::default()),
                Span::styled(
                    "[approve]",
                    Style::default()
                        .fg(crate::theme::status::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", Style::default()),
                Span::styled(
                    "[deny]",
                    Style::default()
                        .fg(crate::theme::status::ERROR)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            if y < inner.y + inner.height {
                let approve_x =
                    inner.x + main_width.saturating_add(2).min(inner.width as usize) as u16;
                let deny_x = inner.x
                    + main_width
                        .saturating_add(2 + "[approve] ".width())
                        .min(inner.width as usize) as u16;
                regions.push(ClickRegion {
                    rect: Rect::new(approve_x, y, "[approve]".width() as u16, 1),
                    target: ClickTarget::RuntimeApproval {
                        approval_id: approval.approval_id.clone(),
                        session_id: approval.command.target.session_id.clone(),
                        approved: true,
                    },
                });
                regions.push(ClickRegion {
                    rect: Rect::new(deny_x, y, "[deny]".width() as u16, 1),
                    target: ClickTarget::RuntimeApproval {
                        approval_id: approval.approval_id.clone(),
                        session_id: approval.command.target.session_id.clone(),
                        approved: false,
                    },
                });
            }
        }
    }

    runtime_push_heading(&mut lines, "Recent Commands");
    if recent_runtime_commands.is_empty() {
        lines.push(runtime_line(
            "no TUI-routed runtime commands",
            inner.width,
            TEXT_DIM,
        ));
    } else {
        for command in recent_runtime_commands.iter().take(4) {
            lines.push(runtime_command_activity_line(command, inner.width));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
    regions
}

fn runtime_push_heading(lines: &mut Vec<Line<'static>>, heading: &str) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            heading.to_string(),
            Style::default()
                .fg(crate::theme::brand::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
}

fn runtime_line(value: &str, width: u16, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        pad_cell(&format!("  {value}"), width as usize),
        Style::default().fg(color),
    ))
}

fn runtime_command_activity_line(activity: &RuntimeCommandActivity, width: u16) -> Line<'static> {
    let target = activity.target.as_deref().unwrap_or("session");
    let age_ms = runtime_dashboard_now_ms().saturating_sub(activity.updated_at_ms);
    let mut text = format!(
        "cmd {} {} target={} age={}ms",
        activity.status, activity.label, target, age_ms
    );
    if let Some(message) = activity
        .message
        .as_deref()
        .filter(|message| !message.is_empty())
    {
        text.push(' ');
        text.push_str(message);
    }
    text.push_str(super::confirmed_state::runtime_command_confirmation_suffix(
        &activity.status,
    ));
    runtime_line(
        &text,
        width,
        runtime_command_activity_status_color(&activity.status),
    )
}
fn runtime_command_activity_status_color(status: &str) -> Color {
    match status {
        "applied" | "accepted" => crate::theme::status::SUCCESS,
        "pending" | "deferred" => crate::theme::status::PENDING,
        "rejected" | "failed" => crate::theme::status::ERROR,
        _ => TEXT_DIM,
    }
}

fn runtime_runtime_view_label(status: &RuntimeDaemonDashboardStatus) -> String {
    status
        .terminal_host
        .as_ref()
        .map(|host| {
            format!(
                "{} / {}",
                runtime_terminal_host_kind_label(host.kind),
                runtime_terminal_host_command_routing_label(host.command_routing)
            )
        })
        .unwrap_or_else(|| "host unknown".to_string())
}

fn runtime_daemon_heartbeat_stale(status: &RuntimeDaemonDashboardStatus) -> bool {
    status.running
        && runtime_daemon_heartbeat_age_ms(status) > RUNTIME_DAEMON_HEARTBEAT_STALE_AFTER_MS
}

fn runtime_daemon_status_display(
    status: &RuntimeDaemonDashboardStatus,
    heartbeat_stale: bool,
) -> (&'static str, Color, &'static str) {
    if heartbeat_stale {
        (
            crate::theme::glyph::WARNING,
            crate::theme::status::WARNING,
            "daemon heartbeat stale",
        )
    } else if status.running {
        (
            crate::theme::glyph::CHECK,
            crate::theme::status::SUCCESS,
            "daemon running",
        )
    } else {
        (
            crate::theme::glyph::CROSS,
            crate::theme::status::ERROR,
            "daemon stopped",
        )
    }
}

fn runtime_sidecar_summary(sidecar: Option<RuntimeSidecarDashboardStatus>) -> String {
    sidecar
        .map(|sidecar| {
            format!(
                "detector={} workflow={}",
                if sidecar.detection_running {
                    "up"
                } else {
                    "down"
                },
                if sidecar.workflow_running {
                    "up"
                } else {
                    "down"
                }
            )
        })
        .unwrap_or_else(|| "sidecar=unknown".to_string())
}

fn runtime_daemon_heartbeat_age_ms(status: &RuntimeDaemonDashboardStatus) -> u64 {
    runtime_dashboard_now_ms().saturating_sub(status.generated_at_ms)
}

fn runtime_dashboard_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(super) fn runtime_terminal_host_summary(
    host: Option<&RuntimeTerminalHostDashboardStatus>,
) -> String {
    let Some(host) = host else {
        return "host=unknown observation=unknown".to_string();
    };
    let mut summary = format!(
        "host={}{} observation={} commands={} pane_owner={} agent_factory={} resize={}",
        runtime_terminal_host_kind_label(host.kind),
        if host.experimental {
            " experimental"
        } else {
            ""
        },
        if host.observation_running {
            "on"
        } else {
            "off"
        },
        runtime_terminal_host_command_routing_label(host.command_routing),
        runtime_terminal_host_pane_ownership_label(host.pane_ownership),
        runtime_terminal_host_agent_factory_label(host.agent_factory),
        runtime_terminal_host_resize_label(host)
    );
    if let Some(session_name) = host.session_name.as_deref() {
        summary.push_str(&format!(" session={session_name}"));
    }
    if let Some(socket_name) = host.socket_name.as_deref() {
        summary.push_str(&format!(" socket={socket_name}"));
    }
    if let Some(socket_dir) = host.socket_dir.as_deref() {
        summary.push_str(&format!(" socket_dir={socket_dir}"));
    }
    if let Some(binary_path) = host.binary_path.as_deref() {
        summary.push_str(&format!(" bin={binary_path}"));
    }
    append_runtime_terminal_host_diagnostics_summary(&mut summary, host);
    summary
}

fn runtime_terminal_host_summary_for_status(status: &RuntimeDaemonDashboardStatus) -> String {
    let Some(host) = status.terminal_host.as_ref() else {
        return "host=unknown observation=unknown".to_string();
    };
    let mut summary = format!(
        "host={} mode={}{} host_panes={} observation={} commands={} pane_owner={} agent_factory={} resize={} promotion={}",
        runtime_terminal_host_kind_label(host.kind),
        runtime_terminal_host_mode_label(status, host),
        if host.experimental {
            " experimental"
        } else {
            ""
        },
        runtime_terminal_host_pane_count(status, host.kind),
        if host.observation_running {
            "on"
        } else {
            "off"
        },
        runtime_terminal_host_command_routing_label(host.command_routing),
        runtime_terminal_host_pane_ownership_label(host.pane_ownership),
        runtime_terminal_host_agent_factory_label(host.agent_factory),
        runtime_terminal_host_resize_label(host),
        if runtime_terminal_host_promotion_readiness(host).ready {
            "ready"
        } else {
            "blocked"
        }
    );
    append_runtime_terminal_host_identity_summary(&mut summary, host);
    append_runtime_terminal_host_diagnostics_summary(&mut summary, host);
    summary
}

fn runtime_terminal_host_identity_summary(host: &RuntimeTerminalHostDashboardStatus) -> String {
    let mut parts = Vec::new();
    if let Some(session_name) = host.session_name.as_deref() {
        parts.push(format!("session={session_name}"));
    }
    if let Some(socket_name) = host.socket_name.as_deref() {
        parts.push(format!("socket={socket_name}"));
    }
    if let Some(socket_dir) = host.socket_dir.as_deref() {
        parts.push(format!("socket_dir={socket_dir}"));
    }
    if let Some(binary_path) = host.binary_path.as_deref() {
        parts.push(format!("bin={binary_path}"));
    }
    if parts.is_empty() {
        "identity=unknown".to_string()
    } else {
        parts.join(" ")
    }
}

pub(crate) fn runtime_terminal_host_attach_command(
    _host: &RuntimeTerminalHostDashboardStatus,
) -> Option<String> {
    None
}

fn append_runtime_terminal_host_identity_summary(
    summary: &mut String,
    host: &RuntimeTerminalHostDashboardStatus,
) {
    if let Some(session_name) = host.session_name.as_deref() {
        summary.push_str(&format!(" session={session_name}"));
    }
    if let Some(socket_name) = host.socket_name.as_deref() {
        summary.push_str(&format!(" socket={socket_name}"));
    }
    if let Some(socket_dir) = host.socket_dir.as_deref() {
        summary.push_str(&format!(" socket_dir={socket_dir}"));
    }
    if let Some(binary_path) = host.binary_path.as_deref() {
        summary.push_str(&format!(" bin={binary_path}"));
    }
}

fn append_runtime_terminal_host_diagnostics_summary(
    summary: &mut String,
    host: &RuntimeTerminalHostDashboardStatus,
) {
    if host.diagnostics.is_empty() {
        return;
    }
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut info = 0usize;
    for diagnostic in &host.diagnostics {
        match diagnostic.severity {
            RuntimeTerminalHostDiagnosticSeverityDashboard::Error => errors += 1,
            RuntimeTerminalHostDiagnosticSeverityDashboard::Warning => warnings += 1,
            RuntimeTerminalHostDiagnosticSeverityDashboard::Info => info += 1,
        }
    }
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(format!("error:{errors}"));
    }
    if warnings > 0 {
        parts.push(format!("warning:{warnings}"));
    }
    if info > 0 {
        parts.push(format!("info:{info}"));
    }
    summary.push_str(&format!(" diagnostics={}", parts.join(",")));
}

fn runtime_terminal_host_mode_label(
    status: &RuntimeDaemonDashboardStatus,
    host: &RuntimeTerminalHostDashboardStatus,
) -> &'static str {
    if host.kind == RuntimeTerminalHostKind::Embedded {
        return "embedded";
    }
    if host.command_routing == RuntimeTerminalHostCommandRoutingDashboard::TerminalHost {
        return "host-owned";
    }
    if runtime_terminal_host_pane_count(status, host.kind) > 0 {
        return "preview";
    }
    "standby"
}

fn runtime_terminal_host_pane_count(
    status: &RuntimeDaemonDashboardStatus,
    kind: RuntimeTerminalHostKind,
) -> usize {
    status
        .registry
        .panes
        .iter()
        .filter(|pane| {
            pane.state != RuntimePaneState::Dead
                && pane
                    .source
                    .as_ref()
                    .is_some_and(|source| runtime_source_matches_terminal_host(kind, source))
        })
        .count()
}

pub(super) fn runtime_registry_summary(status: &RuntimeDaemonDashboardStatus) -> String {
    let ready = status
        .registry
        .panes
        .iter()
        .filter(|pane| pane.state == RuntimePaneState::Ready)
        .count();
    let busy = status
        .registry
        .panes
        .iter()
        .filter(|pane| pane.state == RuntimePaneState::Busy)
        .count();
    let dead = status
        .registry
        .panes
        .iter()
        .filter(|pane| pane.state == RuntimePaneState::Dead)
        .count();
    let unknown = status
        .registry
        .panes
        .iter()
        .filter(|pane| pane.state == RuntimePaneState::Unknown)
        .count();
    let total = status.registry_count.max(status.registry.panes.len());
    let mut summary =
        format!("panes={total} ready={ready} busy={busy} dead={dead} unknown={unknown}");
    let source_summary = runtime_registry_source_summary(status);
    if !source_summary.is_empty() {
        summary.push_str(" sources=");
        summary.push_str(&source_summary);
    }
    summary
}

fn runtime_registry_source_summary(status: &RuntimeDaemonDashboardStatus) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for pane in &status.registry.panes {
        let label = pane
            .source
            .as_ref()
            .map(runtime_source_label)
            .unwrap_or_else(|| "unknown".to_string());
        *counts.entry(label).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(source, count)| format!("{source}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn runtime_registry_preview_panes(
    status: &RuntimeDaemonDashboardStatus,
) -> Vec<&RuntimePaneDashboardInfo> {
    let mut panes: Vec<_> = status.registry.panes.iter().collect();
    panes.sort_by(|left, right| {
        runtime_pane_preview_rank(status, left)
            .cmp(&runtime_pane_preview_rank(status, right))
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| left.pane_id.cmp(&right.pane_id))
    });
    panes
}

fn runtime_pane_preview_rank(
    status: &RuntimeDaemonDashboardStatus,
    pane: &RuntimePaneDashboardInfo,
) -> u8 {
    if let (Some(host), Some(source)) = (status.terminal_host.as_ref(), pane.source.as_ref()) {
        if runtime_source_matches_terminal_host(host.kind, source) {
            return 0;
        }
    }
    match pane.source.as_ref() {
        Some(RuntimeSource::Mux) | None => 2,
        Some(_) => 1,
    }
}

fn runtime_source_matches_terminal_host(
    kind: RuntimeTerminalHostKind,
    source: &RuntimeSource,
) -> bool {
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

fn runtime_registry_table_header(width: u16) -> Line<'static> {
    let columns = RuntimeRegistryColumns::new(width as usize);
    runtime_registry_table_line(
        &columns, "Pane", "Gen", "Kind", "State", "Source", "Output", "Title", TEXT_DIM,
    )
}

fn runtime_registry_table_row(pane: &RuntimePaneDashboardInfo, width: u16) -> Line<'static> {
    let columns = RuntimeRegistryColumns::new(width as usize);
    let pane_label = format!(
        "{}/{}",
        short_runtime_id(&pane.session_id),
        short_runtime_id(&pane.pane_id)
    );
    let generation = pane.generation.to_string();
    let kind = runtime_pane_kind_label(&pane.kind);
    let state = runtime_pane_state_label(&pane.state).to_string();
    let source = pane
        .source
        .as_ref()
        .map(runtime_source_label)
        .unwrap_or_else(|| "unknown".to_string());
    let output = runtime_output_age_label(pane.last_output_ms);
    let title = pane.title.as_deref().unwrap_or("-");
    runtime_registry_table_line(
        &columns,
        &pane_label,
        &generation,
        &kind,
        &state,
        &source,
        &output,
        title,
        Color::White,
    )
}

struct RuntimeRegistryColumns {
    total: usize,
    pane: usize,
    generation: usize,
    kind: usize,
    state: usize,
    source: usize,
    output: usize,
    title: usize,
}

impl RuntimeRegistryColumns {
    fn new(total: usize) -> Self {
        let total = total.max(20);
        let content = total.saturating_sub(2);
        let pane = content.saturating_sub(55).max(20).min(34);
        let generation = 5;
        let kind = 10;
        let state = 8;
        let source = 10;
        let output = 12;
        let separators = 6;
        let fixed = pane + generation + kind + state + source + output + separators;
        let title = content.saturating_sub(fixed).max(8);
        Self {
            total,
            pane,
            generation,
            kind,
            state,
            source,
            output,
            title,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn runtime_registry_table_line(
    columns: &RuntimeRegistryColumns,
    pane: &str,
    generation: &str,
    kind: &str,
    state: &str,
    source: &str,
    output: &str,
    title: &str,
    color: Color,
) -> Line<'static> {
    let row = format!(
        "  {} {} {} {} {} {} {}",
        pad_cell(pane, columns.pane),
        pad_cell(generation, columns.generation),
        pad_cell(kind, columns.kind),
        pad_cell(state, columns.state),
        pad_cell(source, columns.source),
        pad_cell(output, columns.output),
        pad_cell(title, columns.title)
    );
    Line::from(Span::styled(
        pad_cell(&row, columns.total),
        Style::default().fg(color),
    ))
}

fn runtime_pane_summary(pane: &RuntimePaneDashboardInfo) -> String {
    let mut summary = format!(
        "  {}/{} gen={} {} {}",
        short_runtime_id(&pane.session_id),
        short_runtime_id(&pane.pane_id),
        pane.generation,
        runtime_pane_kind_label(&pane.kind),
        runtime_pane_state_label(&pane.state)
    );
    if let Some(source) = pane.source.as_ref() {
        summary.push_str(&format!(" source={}", runtime_source_label(source)));
    }
    if let Some(title) = pane.title.as_deref() {
        summary.push_str(&format!(" title={title:?}"));
    }
    if let Some(last_output_ms) = pane.last_output_ms {
        summary.push_str(&format!(
            " output={}",
            runtime_output_age_label(Some(last_output_ms))
        ));
    }
    if let Some(exit_code) = pane.exit_code {
        summary.push_str(&format!(" exit={exit_code}"));
    }
    if let Some(reason) = pane.exit_reason.as_deref() {
        summary.push_str(&format!(" reason={reason:?}"));
    }
    summary
}

fn runtime_output_age_label(last_output_ms: Option<u64>) -> String {
    runtime_output_age_label_at(runtime_dashboard_now_ms(), last_output_ms)
}

pub(super) fn runtime_output_age_label_at(now_ms: u64, last_output_ms: Option<u64>) -> String {
    let Some(last_output_ms) = last_output_ms else {
        return "-".to_string();
    };
    let age_ms = now_ms.saturating_sub(last_output_ms);
    if age_ms < 1_000 {
        format!("{age_ms}ms ago")
    } else if age_ms < 60_000 {
        format!("{}s ago", age_ms / 1_000)
    } else if age_ms < 60 * 60_000 {
        format!("{}m ago", age_ms / 60_000)
    } else {
        format!("{}h ago", age_ms / (60 * 60_000))
    }
}

fn runtime_terminal_host_kind_label(kind: RuntimeTerminalHostKind) -> &'static str {
    match kind {
        RuntimeTerminalHostKind::Embedded => "embedded",
        RuntimeTerminalHostKind::Headless => "headless",
        RuntimeTerminalHostKind::Web => "web",
        RuntimeTerminalHostKind::NativeGui => "native_gui",
    }
}

fn runtime_terminal_host_command_routing_label(
    routing: RuntimeTerminalHostCommandRoutingDashboard,
) -> &'static str {
    match routing {
        RuntimeTerminalHostCommandRoutingDashboard::Mux => "mux",
        RuntimeTerminalHostCommandRoutingDashboard::TerminalHost => "host",
    }
}

fn runtime_terminal_host_pane_ownership_label(
    ownership: RuntimeTerminalHostPaneOwnership,
) -> &'static str {
    match ownership {
        RuntimeTerminalHostPaneOwnership::Mux => "mux",
        RuntimeTerminalHostPaneOwnership::Host => "host",
    }
}

fn runtime_terminal_host_agent_factory_label(
    routing: RuntimeTerminalHostAgentFactoryRoutingDashboard,
) -> &'static str {
    match routing {
        RuntimeTerminalHostAgentFactoryRoutingDashboard::Mux => "mux",
        RuntimeTerminalHostAgentFactoryRoutingDashboard::TerminalHost => "host",
    }
}

fn runtime_terminal_host_resize_label(host: &RuntimeTerminalHostDashboardStatus) -> &'static str {
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

fn runtime_terminal_host_diagnostic_severity_label(
    severity: RuntimeTerminalHostDiagnosticSeverityDashboard,
) -> &'static str {
    match severity {
        RuntimeTerminalHostDiagnosticSeverityDashboard::Info => "info",
        RuntimeTerminalHostDiagnosticSeverityDashboard::Warning => "warning",
        RuntimeTerminalHostDiagnosticSeverityDashboard::Error => "error",
    }
}

fn runtime_terminal_host_diagnostic_color(
    severity: RuntimeTerminalHostDiagnosticSeverityDashboard,
) -> Color {
    match severity {
        RuntimeTerminalHostDiagnosticSeverityDashboard::Info => TEXT_DIM,
        RuntimeTerminalHostDiagnosticSeverityDashboard::Warning => crate::theme::status::WARNING,
        RuntimeTerminalHostDiagnosticSeverityDashboard::Error => crate::theme::status::ERROR,
    }
}

fn runtime_terminal_host_capabilities_summary(capabilities: &TerminalHostCapabilities) -> String {
    format!(
        "source={} pty={} scrollback={} activity={} resize={} lifecycle={} replay={}",
        runtime_source_label(&capabilities.source),
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

fn runtime_terminal_host_promotion_readiness(
    host: &RuntimeTerminalHostDashboardStatus,
) -> RuntimeTerminalHostPromotionReadinessDashboard {
    if host.promotion_readiness.ready || !host.promotion_readiness.blockers.is_empty() {
        return host.promotion_readiness.clone();
    }

    let mut blockers = Vec::new();
    if host.kind == RuntimeTerminalHostKind::Embedded {
        blockers.push("embedded host is the production default".to_string());
    }
    if host.command_routing != RuntimeTerminalHostCommandRoutingDashboard::TerminalHost {
        blockers.push("daemon commands still route to mux".to_string());
    }
    if host.pane_ownership != RuntimeTerminalHostPaneOwnership::Host {
        blockers.push("agent panes are still mux-owned".to_string());
    }
    if host.agent_factory != RuntimeTerminalHostAgentFactoryRoutingDashboard::TerminalHost {
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
    RuntimeTerminalHostPromotionReadinessDashboard {
        ready: blockers.is_empty(),
        blockers,
    }
}

fn runtime_source_label(source: &RuntimeSource) -> String {
    match source {
        RuntimeSource::Mux => "mux".to_string(),
        RuntimeSource::Daemon => "daemon".to_string(),
        RuntimeSource::EmbeddedTui => "embedded_tui".to_string(),
        RuntimeSource::Web => "web".to_string(),
        RuntimeSource::NativeGui => "native_gui".to_string(),
        RuntimeSource::Headless => "headless".to_string(),
        RuntimeSource::Detector => "detector".to_string(),
        RuntimeSource::Policy => "policy".to_string(),
        RuntimeSource::Other { name } => name.clone(),
    }
}

fn runtime_pane_state_label(state: &RuntimePaneState) -> &'static str {
    match state {
        RuntimePaneState::Ready => "ready",
        RuntimePaneState::Busy => "busy",
        RuntimePaneState::Dead => "dead",
        RuntimePaneState::Unknown => "unknown",
    }
}

fn runtime_pane_kind_label(kind: &RuntimePaneKind) -> String {
    match kind {
        RuntimePaneKind::Supervisor => "supervisor".to_string(),
        RuntimePaneKind::Worker => "worker".to_string(),
        RuntimePaneKind::Reviewer => "reviewer".to_string(),
        RuntimePaneKind::Advisor => "advisor".to_string(),
        RuntimePaneKind::Director => "director".to_string(),
        RuntimePaneKind::Shell => "shell".to_string(),
        RuntimePaneKind::Unknown => "unknown".to_string(),
        RuntimePaneKind::Other { name } => name.clone(),
    }
}

fn short_runtime_id(id: &str) -> String {
    if id.len() <= 18 {
        id.to_string()
    } else {
        format!("{}...", &id[..15])
    }
}

fn runtime_command_summary(command: &RuntimeCommand) -> String {
    let target = command
        .target
        .pane_id
        .as_deref()
        .unwrap_or(command.target.session_id.as_str());
    format!("{} target={target}", runtime_command_label(&command.kind))
}

fn runtime_command_label(kind: &RuntimeCommandKind) -> &'static str {
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

fn dashboard_agent_status(
    pane: &Pane,
    registered: Option<&AgentInfo>,
    tick: usize,
) -> (&'static str, String, StatusKind) {
    dashboard_agent_status_for_state(pane.pane_state(), registered.is_some(), tick)
}

pub(crate) fn dashboard_agent_status_for_state(
    pane_state: Option<&PaneState>,
    registered: bool,
    tick: usize,
) -> (&'static str, String, StatusKind) {
    match pane_state {
        Some(PaneState::Busy { .. }) => (
            "●",
            format!("{} running", crate::theme::glyph::spinner(tick)),
            StatusKind::Running,
        ),
        Some(PaneState::Dead { .. }) => ("✗", "error".to_string(), StatusKind::Error),
        Some(PaneState::Ready { .. }) => ("○", "idle".to_string(), StatusKind::Idle),
        None if registered => ("◐", "starting".to_string(), StatusKind::Info),
        None => ("○", "idle".to_string(), StatusKind::Idle),
    }
}

fn dashboard_agent_identity(pane: &Pane) -> (String, String) {
    match pane.cli_type() {
        AgentAdapter::BuiltIn(cli) => {
            let cli_name = cli.as_str().to_string();
            let provider_name = pane
                .configured_agent_type()
                .unwrap_or(cli.as_str())
                .to_string();
            (cli_name, provider_name)
        }
        AgentAdapter::Custom(custom) => {
            let cli_name = custom
                .command
                .as_deref()
                .unwrap_or(custom.name.as_str())
                .to_string();
            let provider_name = pane
                .configured_agent_type()
                .unwrap_or(custom.name.as_str())
                .to_string();
            (cli_name, provider_name)
        }
    }
}

fn pad_cell(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let fitted = truncate_to(value, width);
    let padding = width.saturating_sub(fitted.width());
    format!("{fitted}{}", " ".repeat(padding))
}

fn infer_assignee_kind(assignee: &str, dashboard: &DashboardData) -> Option<PaneKind> {
    if let Some(agent) = dashboard.agents.iter().find(|agent| agent.name == assignee) {
        return match agent.role.as_str() {
            "worker" => Some(PaneKind::Worker),
            "reviewer" => Some(PaneKind::Reviewer),
            "advisor" => Some(PaneKind::Advisor),
            "supervisor" => Some(PaneKind::Supervisor),
            "director" => Some(PaneKind::Director),
            "shell" => Some(PaneKind::Shell),
            _ => None,
        };
    }

    if assignee.contains("reviewer") {
        Some(PaneKind::Reviewer)
    } else if assignee.contains("supervisor") {
        Some(PaneKind::Supervisor)
    } else if assignee.contains("director") {
        Some(PaneKind::Director)
    } else if assignee.contains("shell") {
        Some(PaneKind::Shell)
    } else if assignee != "—" {
        Some(PaneKind::Worker)
    } else {
        None
    }
}

fn assignee_spans(assignee: &str, dashboard: &DashboardData, width: usize) -> Vec<Span<'static>> {
    let kind = infer_assignee_kind(assignee, dashboard);
    if let Some(kind) = kind {
        let glyph = format!("{} ", crate::theme::role::glyph(&kind));
        let glyph_width = glyph.width();
        let name = truncate_to(assignee, width.saturating_sub(glyph_width));
        let padding = width.saturating_sub(glyph_width + name.width());
        vec![
            Span::styled(glyph, Style::default().fg(crate::theme::role::color(&kind))),
            Span::styled(
                format!("{name}{}", " ".repeat(padding)),
                Style::default().fg(TEXT_DIM),
            ),
        ]
    } else {
        vec![Span::styled(
            pad_cell(assignee, width),
            Style::default().fg(TEXT_DIM),
        )]
    }
}

pub(crate) fn render_dashboard_tasks(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardData,
    expanded_epics: &std::collections::HashSet<String>,
    state: &mut DashboardTaskListState,
) -> Vec<ClickRegion> {
    let mut regions = Vec::new();
    let inner = Panel::new("Tasks")
        .accent(DASH_ACCENT)
        .border(DASH_SECTION_BORDER)
        .right_label(super::confirmed_state::attention_lane_label(dashboard))
        .render(frame, area);
    state.area = inner;
    if dashboard.tasks.is_empty() {
        state.max_scroll = 0;
        state.scroll = 0;
        render_empty_state_card(
            frame,
            inner,
            "Queue",
            "◆ No tasks assigned",
            "wait for supervisor dispatch",
            "initiatives, epics, and tasks appear here",
        );
        return regions;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut row_regions: Vec<DashboardTaskRowRegion> = Vec::new();
    if inner.height >= 7 {
        lines.push(task_scope_summary_line(dashboard, inner.width));
        lines.push(Line::from(""));
    }

    let initiatives: Vec<&TaskInfo> = dashboard
        .tasks
        .iter()
        .filter(|t| t.task_type == "initiative" && t.parent_id.is_none())
        .collect();
    let top_level_epics: Vec<&TaskInfo> = dashboard
        .tasks
        .iter()
        .filter(|t| t.task_type == "epic" && t.parent_id.is_none())
        .collect();
    let orphan_tasks: Vec<&TaskInfo> = dashboard
        .tasks
        .iter()
        .filter(|t| t.task_type == "task" && t.parent_id.is_none())
        .collect();

    let mut children_by_parent: std::collections::HashMap<&str, Vec<&TaskInfo>> =
        std::collections::HashMap::new();
    let tasks_by_id: std::collections::HashMap<&str, &TaskInfo> = dashboard
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), task))
        .collect();
    for task in &dashboard.tasks {
        if let Some(ref pid) = task.parent_id {
            children_by_parent
                .entry(pid.as_str())
                .or_default()
                .push(task);
        }
    }

    fn render_leaf_row(
        lines: &mut Vec<Line>,
        row_regions: &mut Vec<DashboardTaskRowRegion>,
        task: &TaskInfo,
        depth: usize,
        dashboard: &DashboardData,
        tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
        row_width: u16,
    ) {
        let status = compute_display_status(task);
        let assignee = task.assignee.as_deref().unwrap_or("—");
        let indent = format!("{}└ ", "  ".repeat(depth + 1));
        let blocked_hint = task_dashboard_hint(task, tasks_by_id);
        let line_idx = lines.len() as u16;

        let fixed_cols = indent.chars().count() + 14 + 16 + 18; // indent + id + status + assignee
        let title_budget = (row_width as usize).saturating_sub(fixed_cols + 1);
        let blocked_len = blocked_hint.as_ref().map_or(0, |h| h.chars().count());
        let title_max = title_budget.saturating_sub(blocked_len);
        let title = truncate_to(&task.title, title_max);

        let mut spans = vec![
            Span::styled(indent, Style::default().fg(TEXT_DIM)),
            Span::styled(
                format!("{:<14}", task.id),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("{:<16}", status),
                crate::theme::status_style(task_status_kind(&status)),
            ),
        ];
        spans.extend(assignee_spans(assignee, dashboard, 18));
        spans.push(Span::styled(title, Style::default().fg(Color::White)));
        if let Some(blocked_hint) = blocked_hint {
            spans.push(Span::styled(
                blocked_hint,
                Style::default().fg(crate::theme::detail::TASK_HINT),
            ));
        }
        lines.push(Line::from(spans));
        row_regions.push(DashboardTaskRowRegion {
            line_idx,
            x: 0,
            width: u16::MAX,
            target: ClickTarget::TaskDetail(task.id.clone()),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn render_container_row(
        lines: &mut Vec<Line>,
        row_regions: &mut Vec<DashboardTaskRowRegion>,
        container: &TaskInfo,
        depth: usize,
        dashboard: &DashboardData,
        expanded: &std::collections::HashSet<String>,
        children_by_parent: &std::collections::HashMap<&str, Vec<&TaskInfo>>,
        tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
        row_width: u16,
    ) {
        let is_expanded = expanded.contains(&container.id);
        let children = children_by_parent.get(container.id.as_str());
        let (total, done) = children.map_or((0, 0), |kids| {
            let total = kids.len();
            let done = kids
                .iter()
                .filter(|kid| task_counts_toward_completion(kid))
                .count();
            (total, done)
        });
        let progress = if total > 0 {
            format!(" [{done}/{total}]")
        } else {
            String::new()
        };
        let tokens = if container.tokens_used > 0 {
            format!("  {} tok", format_token_count(container.tokens_used))
        } else {
            String::new()
        };
        let arrow = if is_expanded {
            crate::theme::glyph::ARROW_DOWN
        } else {
            crate::theme::glyph::ARROW_RIGHT
        };
        let status = compute_display_status(container);
        let indent = format!("{}{} ", "  ".repeat(depth + 1), arrow);
        let line_idx = lines.len() as u16;

        // Truncate title to fit
        let fixed_cols =
            indent.chars().count() + 14 + 16 + progress.chars().count() + tokens.chars().count();
        let title_budget = (row_width as usize).saturating_sub(fixed_cols + 1);
        let title = truncate_to(&container.title, title_budget);

        lines.push(Line::from(vec![
            Span::styled(indent.clone(), Style::default().fg(DASH_ACCENT)),
            Span::styled(
                format!("{:<14}", container.id),
                Style::default()
                    .fg(DASH_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<16}", status),
                crate::theme::status_style(task_status_kind(&status)),
            ),
            Span::styled(
                title,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(progress, Style::default().fg(TEXT_DIM)),
            Span::styled(tokens, Style::default().fg(TEXT_DIM)),
        ]));

        let indent_width = indent.width() as u16;
        row_regions.push(DashboardTaskRowRegion {
            line_idx,
            x: indent_width,
            width: 14,
            target: ClickTarget::TaskDetail(container.id.clone()),
        });
        row_regions.push(DashboardTaskRowRegion {
            line_idx,
            x: 0,
            width: u16::MAX,
            target: ClickTarget::EpicToggle(container.id.clone()),
        });

        if is_expanded {
            if let Some(children) = children {
                for child in children {
                    if task_is_container(child) {
                        render_container_row(
                            lines,
                            row_regions,
                            child,
                            depth + 1,
                            dashboard,
                            expanded,
                            children_by_parent,
                            tasks_by_id,
                            row_width,
                        );
                    } else {
                        render_leaf_row(
                            lines,
                            row_regions,
                            child,
                            depth + 1,
                            dashboard,
                            tasks_by_id,
                            row_width,
                        );
                    }
                }
            }
        }
    }

    for initiative in &initiatives {
        render_container_row(
            &mut lines,
            &mut row_regions,
            initiative,
            0,
            dashboard,
            expanded_epics,
            &children_by_parent,
            &tasks_by_id,
            inner.width,
        );
    }

    for epic in &top_level_epics {
        render_container_row(
            &mut lines,
            &mut row_regions,
            epic,
            0,
            dashboard,
            expanded_epics,
            &children_by_parent,
            &tasks_by_id,
            inner.width,
        );
    }

    if !orphan_tasks.is_empty() {
        if !initiatives.is_empty() || !top_level_epics.is_empty() {
            lines.push(Line::from(""));
        }
        for task in &orphan_tasks {
            let task_display_status = compute_display_status(task);
            let assignee = task.assignee.as_deref().unwrap_or("—");
            let blocked_hint = task_dashboard_hint(task, &tasks_by_id);
            let line_idx = lines.len() as u16;

            let fixed_cols = 2 + 14 + 16 + 18; // indent + id + status + assignee
            let title_budget = (inner.width as usize).saturating_sub(fixed_cols + 1);
            let blocked_len = blocked_hint.as_ref().map_or(0, |h| h.chars().count());
            let title_max = title_budget.saturating_sub(blocked_len);
            let title = truncate_to(&task.title, title_max);

            let mut spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{:<14}", task.id),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("{:<16}", task_display_status),
                    crate::theme::status_style(task_status_kind(&task_display_status)),
                ),
            ];
            spans.extend(assignee_spans(assignee, dashboard, 18));
            spans.push(Span::styled(title, Style::default().fg(Color::White)));
            if let Some(blocked_hint) = blocked_hint {
                spans.push(Span::styled(
                    blocked_hint,
                    Style::default().fg(crate::theme::detail::TASK_HINT),
                ));
            }
            lines.push(Line::from(spans));
            row_regions.push(DashboardTaskRowRegion {
                line_idx,
                x: 0,
                width: u16::MAX,
                target: ClickTarget::TaskDetail(task.id.clone()),
            });
        }
    }

    let max_scroll = lines.len().saturating_sub(inner.height as usize) as u16;
    state.max_scroll = max_scroll;
    state.scroll = state.scroll.min(max_scroll);

    frame.render_widget(Paragraph::new(lines).scroll((state.scroll, 0)), inner);

    for spec in row_regions {
        let Some(relative_y) = spec.line_idx.checked_sub(state.scroll) else {
            continue;
        };
        if relative_y >= inner.height {
            continue;
        }

        let x_offset = spec.x.min(inner.width);
        let width = if spec.width == u16::MAX {
            inner.width.saturating_sub(x_offset)
        } else {
            spec.width.min(inner.width.saturating_sub(x_offset))
        };
        if width == 0 {
            continue;
        }

        regions.push(ClickRegion {
            rect: Rect::new(inner.x + x_offset, inner.y + relative_y, width, 1),
            target: spec.target,
        });
    }
    regions
}

fn render_dashboard_activity(frame: &mut Frame, area: Rect, dashboard: &DashboardData) {
    let inner = Panel::new("Activity")
        .accent(DASH_ACCENT)
        .border(DASH_SECTION_BORDER)
        .render(frame, area);

    if dashboard.events.is_empty() {
        render_empty_state_card(
            frame,
            inner,
            "Activity",
            "◌ No recent events",
            "waiting for lifecycle activity",
            "resets, deliveries, recoveries, and crashes stream here",
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for event in dashboard.events.iter().rev().take(inner.height as usize) {
        let kind = classify_activity_event(&event.description);
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}  ", render_activity_timestamp(&event.timestamp)),
                Style::default().fg(TEXT_MUTED),
            ),
            Span::styled(
                format!("{}  ", activity_glyph(kind)),
                Style::default().fg(activity_color(kind)),
            ),
            Span::styled(
                event.description.clone(),
                Style::default().fg(crate::theme::chrome::TEXT),
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

#[derive(Clone, Copy)]
enum ActivityEventKind {
    Success,
    Running,
    Highlight,
    Warning,
    Error,
}

fn classify_activity_event(description: &str) -> ActivityEventKind {
    let lower = description.to_ascii_lowercase();
    if lower.contains("failed") || lower.contains("error") || lower.contains("dead-lettered") {
        ActivityEventKind::Error
    } else if lower.contains("warning")
        || lower.contains("blocked")
        || lower.contains("stale")
        || lower.contains("timed out")
    {
        ActivityEventKind::Warning
    } else if lower.contains("running")
        || lower.contains("continuing")
        || lower.contains("reloading")
        || lower.contains("retry")
        || lower.contains("reset ")
    {
        ActivityEventKind::Running
    } else if lower.contains("ready")
        || lower.contains("completed")
        || lower.contains("registered")
        || lower.contains("delivered")
        || lower.contains("submitted")
    {
        ActivityEventKind::Success
    } else {
        ActivityEventKind::Highlight
    }
}

fn activity_glyph(kind: ActivityEventKind) -> &'static str {
    match kind {
        ActivityEventKind::Success => crate::theme::glyph::CHECK,
        ActivityEventKind::Running => crate::theme::glyph::RUNNING,
        ActivityEventKind::Highlight => crate::theme::glyph::DIAMOND,
        ActivityEventKind::Warning => crate::theme::glyph::WARNING,
        ActivityEventKind::Error => crate::theme::glyph::CROSS,
    }
}

fn activity_color(kind: ActivityEventKind) -> Color {
    match kind {
        ActivityEventKind::Success => crate::theme::status::SUCCESS,
        ActivityEventKind::Running => crate::theme::status::RUNNING,
        ActivityEventKind::Highlight => crate::theme::status::INFO,
        ActivityEventKind::Warning => crate::theme::status::WARNING,
        ActivityEventKind::Error => crate::theme::status::ERROR,
    }
}

fn render_activity_timestamp(timestamp: &str) -> String {
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        return parsed.format("%M:%S").to_string();
    }
    if let Ok(parsed) = chrono::NaiveTime::parse_from_str(timestamp, "%H:%M:%S") {
        return parsed.format("%M:%S").to_string();
    }
    if let Ok(parsed) = chrono::NaiveTime::parse_from_str(timestamp, "%H:%M") {
        return parsed.format("%H:%M").to_string();
    }
    timestamp.to_string()
}

fn read_research_context(v: &serde_json::Value) -> Vec<ResearchContextInfo> {
    v.get("research_context")
        .and_then(|value| value.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some(ResearchContextInfo {
                        artifact_id: read_optional_string(entry, "artifact_id")?,
                        role: read_optional_string(entry, "role")
                            .unwrap_or_else(|| "research".to_string()),
                        title: read_optional_string(entry, "title")
                            .unwrap_or_else(|| "Research artifact".to_string()),
                        summary: read_optional_string(entry, "summary").unwrap_or_default(),
                        artifact_path: read_optional_string(entry, "artifact_path"),
                        confidence: read_optional_string(entry, "confidence"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read all task files from `.brehon/runtime/tasks/*.json` for the dashboard.
pub(crate) fn read_task_files(brehon_root: &std::path::Path) -> Vec<TaskInfo> {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    let review_contexts = read_review_contexts(brehon_root);
    let Ok(entries) = std::fs::read_dir(&tasks_dir) else {
        return Vec::new();
    };
    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                let id = v["task_id"].as_str().unwrap_or_default().to_string();
                let title = v["title"].as_str().unwrap_or_default().to_string();
                let status = v["status"].as_str().unwrap_or("pending").to_string();
                let assignee = v["assignee"].as_str().map(String::from);
                let task_type = v["task_type"].as_str().unwrap_or("task").to_string();
                let parent_id = v["parent_id"].as_str().map(String::from);
                if !id.is_empty() {
                    let review_context = review_contexts.get(&id);
                    let review_panel_lease_state = if let Some(context) = review_context {
                        if context.has_lease {
                            let task_status = normalize_task_status(&status).unwrap_or("unknown");
                            Some(match task_status {
                                "in_review" => "collecting".to_string(),
                                "approved" => "approved_pending_terminal".to_string(),
                                "changes_requested" | "in_progress" | "pending" | "assigned"
                                | "blocked" => "leased_waiting_for_revision".to_string(),
                                "merged" | "closed" => "terminal_release_pending".to_string(),
                                _ => "leased".to_string(),
                            })
                        } else if context.review_panel_id.is_some() {
                            Some("missing".to_string())
                        } else {
                            None
                        }
                    } else if normalize_task_status(&status) == Some("in_review") {
                        Some("awaiting_panel".to_string())
                    } else {
                        None
                    };
                    let proof_summary = read_proof_summary_for(&id);
                    let feedback_summary = read_feedback_summary_for(&id);
                    tasks.push(TaskInfo {
                        id,
                        title,
                        status,
                        assignee,
                        task_type,
                        parent_id,
                        description: read_optional_string(&v, "description").unwrap_or_default(),
                        priority: read_optional_string(&v, "priority"),
                        percent: read_optional_u64(&v, "percent"),
                        tokens_used: v
                            .get("token_usage")
                            .and_then(|usage| read_optional_u64(usage, "tokens_used"))
                            .or_else(|| read_optional_u64(&v, "tokens_used"))
                            .unwrap_or(0),
                        completion_mode: read_optional_string(&v, "completion_mode"),
                        merge_target: read_optional_string(&v, "merge_target"),
                        integration_status: read_optional_string(&v, "integration_status"),
                        integration_branch: read_optional_string(&v, "integration_branch"),
                        integration_worktree: read_optional_string(&v, "integration_worktree"),
                        activity: read_optional_string(&v, "activity"),
                        notes: read_optional_string(&v, "notes"),
                        blockers: read_optional_string(&v, "blockers"),
                        dependencies: read_string_list(&v, "dependencies"),
                        blocked_by: read_string_list(&v, "blocked_by"),
                        created_at: read_optional_string(&v, "created_at"),
                        updated_at: read_optional_string(&v, "updated_at"),
                        closed_at: read_optional_string(&v, "closed_at"),
                        closed_by: read_optional_string(&v, "closed_by"),
                        merged_commit: read_optional_string(&v, "merged_commit"),
                        merged_branch: read_optional_string(&v, "merged_branch"),
                        latest_commit: read_optional_string(&v, "latest_commit"),
                        run: read_task_run_info(&v),
                        review_id: review_context.and_then(|context| context.review_id.clone()),
                        review_status: review_context
                            .and_then(|context| context.review_status.clone()),
                        review_round: review_context.and_then(|context| context.review_round),
                        review_panel_id: review_context
                            .and_then(|context| context.review_panel_id.clone()),
                        review_panel_members: review_context
                            .map(|context| context.review_panel_members.clone())
                            .unwrap_or_default(),
                        review_panel_lease_state,
                        review_feedback_outcome: v
                            .get("review_feedback")
                            .and_then(|feedback| read_optional_string(feedback, "outcome")),
                        review_feedback_threshold_reason: v.get("review_feedback").and_then(
                            |feedback| read_optional_string(feedback, "threshold_reason"),
                        ),
                        review_feedback_evaluated_at: v
                            .get("review_feedback")
                            .and_then(|feedback| read_optional_string(feedback, "evaluated_at")),
                        review_feedback_blocking: v
                            .get("review_feedback")
                            .map_or_else(Vec::new, |feedback| {
                                read_review_finding_summaries(feedback, "blocking")
                            }),
                        review_feedback_suggestions: v
                            .get("review_feedback")
                            .map_or_else(Vec::new, |feedback| {
                                read_review_finding_summaries(feedback, "suggestions")
                            }),
                        review_feedback_nitpicks: v
                            .get("review_feedback")
                            .map_or_else(Vec::new, |feedback| {
                                read_review_finding_summaries(feedback, "nitpicks")
                            }),
                        review_feedback_dissent: v
                            .get("review_feedback")
                            .map_or_else(Vec::new, |feedback| {
                                read_string_list(feedback, "dissent")
                            }),
                        integration_conflict_owner: v
                            .get("integration_conflict")
                            .and_then(|conflict| read_optional_string(conflict, "owner")),
                        integration_conflict_source: v
                            .get("integration_conflict")
                            .and_then(|conflict| read_optional_string(conflict, "source")),
                        integration_conflict_merge_target: v
                            .get("integration_conflict")
                            .and_then(|conflict| read_optional_string(conflict, "merge_target")),
                        integration_conflict_reviewed_commit: v
                            .get("integration_conflict")
                            .and_then(|conflict| read_optional_string(conflict, "reviewed_commit")),
                        integration_conflict_previous_worker: v
                            .get("integration_conflict")
                            .and_then(|conflict| read_optional_string(conflict, "previous_worker")),
                        integration_conflict_conflicting_files: v
                            .get("integration_conflict")
                            .map_or_else(Vec::new, |conflict| {
                                read_string_list(conflict, "conflicting_files")
                            }),
                        acceptance_criteria: read_string_list(&v, "acceptance_criteria"),
                        file_hints: read_string_list(&v, "file_hints"),
                        constraints: read_string_list(&v, "constraints"),
                        test_requirements: read_string_list(&v, "test_requirements"),
                        plan_steps: read_string_list(&v, "plan_steps"),
                        implementation_notes: read_optional_string(&v, "implementation_notes"),
                        research_context: read_research_context(&v),
                        proof: proof_summary,
                        feedback: feedback_summary,
                    });
                }
            }
        }
    }
    filter_dashboard_tasks(tasks)
}

pub(crate) fn filter_dashboard_tasks(tasks: Vec<TaskInfo>) -> Vec<TaskInfo> {
    let visible_containers: std::collections::HashSet<String> = tasks
        .iter()
        .filter(|task| task_is_container(task))
        .filter(|container| {
            !task_is_terminal(container)
                || has_active_descendant(&tasks, &container.id)
                || has_nonterminal_container_ancestor(&tasks, container)
        })
        .map(|task| task.id.clone())
        .collect();

    tasks
        .into_iter()
        .filter(|task| {
            if task_is_container(task) {
                return visible_containers.contains(&task.id);
            }
            if let Some(parent_id) = task.parent_id.as_deref() {
                return visible_containers.contains(parent_id);
            }
            !task_is_terminal(task)
        })
        .collect()
}
