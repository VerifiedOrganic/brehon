//! Research room rendering and operator request enqueueing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brehon_types::{
    task::is_terminal_task_status, BrehonConfig, ResearchConfig, ResearchPoolConfig,
};
use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::components::Panel;
use crate::theme::chrome::{self, TEXT_DIM, TEXT_MUTED};

use super::composer::enqueue_composer_message;
use super::rendering::truncate_to;
use super::session::read_session_files;
use super::types::ResearchRoomViewState;

/// Loads the merged `BrehonConfig` for a project given its `.brehon` root.
///
/// brehon-tui cannot depend on `brehon-config` (per the dependency-boundary
/// policy), so the loader is injected from `brehon-cli` at startup. The
/// closure should handle stripping `.brehon` from the path before consulting
/// the config layers. Returns `None` if the config file is missing or fails
/// to parse — callers fall back to sensible defaults so the TUI keeps
/// rendering rather than crashing.
pub type ProjectConfigLoader = Arc<dyn Fn(&Path) -> Option<BrehonConfig> + Send + Sync>;

const JOB_STATUS_QUEUED: &str = "queued";
const JOB_STATUS_RUNNING: &str = "running";
const JOB_STATUS_COMPLETED: &str = "completed";
const OPERATOR_REQUEST_TEMPLATE: &str = "operator-request";

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ResearchJobFile {
    job_id: String,
    task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route_id: Option<String>,
    template_id: String,
    pool: String,
    lane: String,
    role: String,
    #[serde(default = "default_job_status")]
    status: String,
    #[serde(default = "default_job_origin")]
    origin: String,
    prompt: String,
    #[serde(default)]
    cost_units: u32,
    #[serde(default = "default_requested_by")]
    requested_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assigned_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(default = "Utc::now")]
    created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ResearchManifestFile {
    task_id: String,
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    artifacts: Vec<ResearchArtifactFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct ResearchArtifactFile {
    artifact_id: String,
    job_id: String,
    pool: String,
    role: String,
    title: String,
    summary: String,
    #[serde(default)]
    confidence: Option<String>,
    artifact_path: String,
    #[serde(default)]
    structured_path: String,
    #[serde(default)]
    citations: Vec<String>,
    #[serde(default)]
    supersedes: Vec<String>,
    #[serde(default)]
    handoff_deliveries: Vec<ResearchHandoffDeliveryFile>,
    #[serde(default)]
    handoff_warnings: Vec<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ResearchHandoffDeliveryFile {
    target: String,
    target_role: String,
    status: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    prompt_id: Option<String>,
    #[serde(default)]
    warning: Option<String>,
}

#[derive(Debug, Clone)]
struct ResearchRoomFile {
    task_id: String,
    title: Option<String>,
    jobs: Vec<ResearchJobFile>,
    artifacts: Vec<ResearchArtifactFile>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct ResearchEvent {
    seq: usize,
    author: String,
    role: String,
    kind: String,
    status: Option<String>,
    pool: Option<String>,
    title: Option<String>,
    content: String,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResearchPostResult {
    pub task_id: String,
    pub job_id: String,
    pub notified_agents: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ResearchPanelSummary {
    live_agents: usize,
    rooms: usize,
    queued_jobs: usize,
    running_jobs: usize,
    completed_jobs: usize,
    artifacts: usize,
}

impl ResearchPanelSummary {
    fn from_rooms(brehon_root: Option<&Path>, rooms: &[ResearchRoomFile]) -> Self {
        Self {
            live_agents: live_research_agent_count(brehon_root),
            rooms: rooms.len(),
            queued_jobs: rooms
                .iter()
                .flat_map(|room| room.jobs.iter())
                .filter(|job| job.status == JOB_STATUS_QUEUED)
                .count(),
            running_jobs: rooms
                .iter()
                .flat_map(|room| room.jobs.iter())
                .filter(|job| job.status == JOB_STATUS_RUNNING)
                .count(),
            completed_jobs: rooms
                .iter()
                .flat_map(|room| room.jobs.iter())
                .filter(|job| job.status == JOB_STATUS_COMPLETED)
                .count(),
            artifacts: rooms.iter().map(|room| room.artifacts.len()).sum(),
        }
    }

    fn header_label(&self) -> String {
        format!(
            "{} agents / {} rooms / {} queued / {} running / {} done / {} artifacts",
            self.live_agents,
            self.rooms,
            self.queued_jobs,
            self.running_jobs,
            self.completed_jobs,
            self.artifacts
        )
    }
}

fn live_research_agent_count(brehon_root: Option<&Path>) -> usize {
    brehon_root
        .map(read_session_files)
        .map(|sessions| {
            sessions
                .values()
                .filter(|(role, _, _)| role == "research")
                .count()
        })
        .unwrap_or(0)
}

fn default_job_status() -> String {
    JOB_STATUS_QUEUED.to_string()
}

fn default_job_origin() -> String {
    "manual_request".to_string()
}

fn default_requested_by() -> String {
    "operator".to_string()
}

pub(crate) fn research_room_count(
    brehon_root: Option<&Path>,
    loader: &ProjectConfigLoader,
) -> usize {
    read_research_rooms(brehon_root, loader).len()
}

pub(crate) fn active_research_room_task_id(
    brehon_root: Option<&Path>,
    loader: &ProjectConfigLoader,
) -> Option<String> {
    read_research_rooms(brehon_root, loader)
        .into_iter()
        .next()
        .map(|room| room.task_id)
}

pub(crate) fn post_operator_research_request(
    brehon_root: &Path,
    loader: &ProjectConfigLoader,
    default_task_id: Option<&str>,
    content: &str,
    session_name: Option<&str>,
) -> Result<ResearchPostResult, String> {
    let config = loader(brehon_root).ok_or_else(|| {
        format!(
            "failed to load project config from {}",
            brehon_root.display()
        )
    })?;
    if !config.research.enabled {
        return Err(
            "research.enabled is false; enable research before posting requests".to_string(),
        );
    }
    let pool = config
        .research
        .pools
        .first()
        .ok_or_else(|| "research has no pools configured".to_string())?;
    let (task_id, prompt) = parse_operator_request(default_task_id, content)?;
    let task = read_task(brehon_root, &task_id)
        .ok_or_else(|| format!("task '{task_id}' not found in runtime task state"))?;
    let task_id = task_id_from_task(&task)
        .unwrap_or(task_id.as_str())
        .to_string();
    let root = research_root_from_config(brehon_root, &config.research)?;
    let jobs_dir = root.join(sanitize_id(&task_id)).join("jobs");
    let job_id = next_job_id(&jobs_dir, &task_id, OPERATOR_REQUEST_TEMPLATE)?;
    let now = Utc::now();
    let job = ResearchJobFile {
        job_id: job_id.clone(),
        task_id: task_id.clone(),
        route_id: None,
        template_id: OPERATOR_REQUEST_TEMPLATE.to_string(),
        pool: pool.id.clone(),
        lane: pool.lane.clone(),
        role: pool.role.clone(),
        status: JOB_STATUS_QUEUED.to_string(),
        origin: "manual_request".to_string(),
        prompt,
        cost_units: pool.cost_units,
        requested_by: "operator".to_string(),
        assigned_to: None,
        artifact_id: None,
        depends_on: Vec::new(),
        warnings: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    write_job(
        &jobs_dir.join(format!("{}.json", sanitize_id(&job_id))),
        &job,
    )?;
    let notified_agents = notify_research_agents(brehon_root, session_name, pool, &job);
    Ok(ResearchPostResult {
        task_id,
        job_id,
        notified_agents,
    })
}

pub(crate) fn render_research_view(
    frame: &mut Frame,
    area: Rect,
    brehon_root: Option<&Path>,
    loader: &ProjectConfigLoader,
    state: &mut ResearchRoomViewState,
) {
    let selected_task_id = state.selected_task_id.clone();
    let mut rooms =
        read_research_rooms_for_render(brehon_root, loader, selected_task_id.as_deref());
    if let (Some(root), Some(task_id)) = (brehon_root, state.selected_task_id.as_deref()) {
        if !rooms.iter().any(|room| room.task_id == task_id) {
            rooms.push(ResearchRoomFile {
                task_id: task_id.to_string(),
                title: read_task_title(root, task_id),
                jobs: Vec::new(),
                artifacts: Vec::new(),
                updated_at: None,
            });
        }
    }
    let summary = ResearchPanelSummary::from_rooms(brehon_root, &rooms);
    let header_label = summary.header_label();
    let inner = Panel::new("Research")
        .subtitle(&header_label)
        .render(frame, area);

    if inner.width == 0 || inner.height == 0 {
        state.area = inner;
        state.max_scroll = 0;
        state.scroll = 0;
        return;
    }

    if rooms.is_empty() {
        state.area = inner;
        state.max_scroll = 0;
        state.scroll = 0;
        let lines = empty_state_lines(brehon_root, loader, inner.width);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let columns = if inner.width > 86 {
        let list_width = (inner.width / 3).clamp(38, 54);
        Layout::horizontal([Constraint::Length(list_width), Constraint::Min(30)]).split(inner)
    } else {
        Layout::horizontal([Constraint::Percentage(100)]).split(inner)
    };
    let room_list = columns[0];
    let detail = if columns.len() > 1 {
        columns[1]
    } else {
        columns[0]
    };
    let selected_idx = state
        .selected_task_id
        .as_deref()
        .and_then(|task_id| rooms.iter().position(|room| room.task_id == task_id))
        .unwrap_or(0);
    state.selected_task_id = Some(rooms[selected_idx].task_id.clone());
    let selected = &rooms[selected_idx];

    if columns.len() > 1 {
        render_room_list(frame, room_list, &rooms, selected_idx);
    }
    render_room_detail(frame, detail, selected, brehon_root, state);
}

fn empty_state_lines(
    brehon_root: Option<&Path>,
    loader: &ProjectConfigLoader,
    width: u16,
) -> Vec<Line<'static>> {
    let config_hint = match brehon_root.and_then(|root| loader(root)) {
        Some(config) if config.research.enabled && config.research.pools.is_empty() => {
            "research.enabled is true, but no pools are configured."
        }
        Some(config) if config.research.enabled => {
            "No jobs or artifacts yet. Press Ctrl-o, then send `/task T-123 <question>`."
        }
        Some(_) => "Enable the commented research block in .brehon/config.yaml to queue requests.",
        None => "No .brehon root is available for research state.",
    };

    vec![
        Line::from(Span::styled(
            "No research rooms yet.",
            Style::default()
                .fg(chrome::TEXT_BODY)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            truncate_to(config_hint, width as usize),
            Style::default().fg(TEXT_DIM),
        )),
        Line::from(Span::styled(
            "Requests write a queued research job and return immediately; agent replies arrive as artifacts.",
            Style::default().fg(TEXT_MUTED),
        )),
    ]
}

fn render_room_list(
    frame: &mut Frame,
    area: Rect,
    rooms: &[ResearchRoomFile],
    selected_idx: usize,
) {
    let mut lines = Vec::new();
    for (idx, room) in rooms.iter().take(area.height as usize / 2 + 1).enumerate() {
        let active = idx == selected_idx;
        let label = room.title.as_deref().unwrap_or(&room.task_id);
        let prefix = if active { "> " } else { "  " };
        let style = if active {
            Style::default()
                .fg(crate::theme::brand::PRIMARY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(chrome::TEXT_BODY)
        };
        lines.push(Line::from(Span::styled(
            truncate_to(&format!("{prefix}{label}"), area.width as usize),
            style,
        )));
        if lines.len() < area.height as usize {
            lines.push(Line::from(status_spans(room, area.width as usize, active)));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn status_spans(room: &ResearchRoomFile, width: usize, active: bool) -> Vec<Span<'static>> {
    let queued = room
        .jobs
        .iter()
        .filter(|job| job.status == JOB_STATUS_QUEUED)
        .count();
    let running = room
        .jobs
        .iter()
        .filter(|job| job.status == JOB_STATUS_RUNNING)
        .count();
    let completed = room
        .jobs
        .iter()
        .filter(|job| job.status == JOB_STATUS_COMPLETED)
        .count();
    let task_width = width.saturating_sub(25).max(8);
    let mut spans = vec![
        Span::styled(
            truncate_to(&format!("  {}", room.task_id), task_width),
            Style::default().fg(TEXT_DIM),
        ),
        Span::raw(" "),
        Span::styled(format!("q{queued}"), Style::default().fg(TEXT_MUTED)),
        Span::raw(" "),
        Span::styled(
            format!("r{running}"),
            Style::default().fg(if running > 0 {
                crate::theme::status::WARNING
            } else {
                TEXT_MUTED
            }),
        ),
        Span::raw(" "),
        Span::styled(
            format!("d{completed}"),
            Style::default().fg(if completed > 0 {
                crate::theme::status::SUCCESS
            } else {
                TEXT_MUTED
            }),
        ),
        Span::raw(" "),
        Span::styled(
            format!("a{}", room.artifacts.len()),
            Style::default().fg(if room.artifacts.is_empty() {
                TEXT_MUTED
            } else {
                crate::theme::brand::PRIMARY
            }),
        ),
    ];
    if active {
        spans.insert(
            0,
            Span::styled("  ", Style::default().fg(crate::theme::brand::PRIMARY)),
        );
    }
    spans
}

fn render_room_detail(
    frame: &mut Frame,
    area: Rect,
    room: &ResearchRoomFile,
    brehon_root: Option<&Path>,
    state: &mut ResearchRoomViewState,
) {
    state.area = area;
    let lines = room_detail_lines(room, brehon_root, area.width);
    let max_scroll = lines.len().saturating_sub(area.height as usize) as u16;
    state.max_scroll = max_scroll;
    state.scroll = state.scroll.min(max_scroll);
    frame.render_widget(Paragraph::new(lines).scroll((state.scroll, 0)), area);
}

fn room_detail_lines(
    room: &ResearchRoomFile,
    brehon_root: Option<&Path>,
    width: u16,
) -> Vec<Line<'static>> {
    let title = room.title.as_deref().unwrap_or(&room.task_id);
    let latest = room
        .updated_at
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut lines = vec![
        Line::from(vec![Span::styled(
            truncate_to(title, width as usize),
            Style::default()
                .fg(chrome::TEXT_BODY)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            truncate_to(
                &format!(
                    "task {} / updated {} / {} jobs / {} artifacts",
                    room.task_id,
                    latest,
                    room.jobs.len(),
                    room.artifacts.len()
                ),
                width as usize,
            ),
            Style::default().fg(TEXT_DIM),
        )]),
        Line::from(Span::styled(
            truncate_to(
                "Ctrl-o opens the request bar. Replies attach to task.research_context.",
                width as usize,
            ),
            Style::default().fg(TEXT_MUTED),
        )),
        Line::from(""),
    ];

    let events = research_events(room, brehon_root);
    if events.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_to("No requests or artifacts in this room yet.", width as usize),
            Style::default().fg(TEXT_MUTED),
        )));
        return lines;
    }
    for (idx, event) in events.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(research_event_block_lines(event, width as usize));
    }
    lines
}

fn research_events(room: &ResearchRoomFile, brehon_root: Option<&Path>) -> Vec<ResearchEvent> {
    let mut events = Vec::new();
    for job in &room.jobs {
        let has_artifact = room
            .artifacts
            .iter()
            .any(|artifact| artifact.job_id == job.job_id);
        if job.status == JOB_STATUS_COMPLETED && has_artifact {
            continue;
        }
        events.push(ResearchEvent {
            seq: 0,
            author: if job.requested_by.trim().is_empty() {
                "operator".to_string()
            } else {
                job.requested_by.clone()
            },
            role: "request".to_string(),
            kind: job.origin.replace('_', "-"),
            status: Some(job.status.clone()),
            pool: Some(job.pool.clone()),
            title: Some(job.template_id.clone()),
            content: request_preview(job),
            created_at: Some(job.created_at),
        });
    }
    for artifact in &room.artifacts {
        let job = room.jobs.iter().find(|job| job.job_id == artifact.job_id);
        let author = job
            .and_then(|job| job.assigned_to.as_deref())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&artifact.pool)
            .to_string();
        events.push(ResearchEvent {
            seq: 0,
            author,
            role: artifact.role.clone(),
            kind: "artifact".to_string(),
            status: Some(artifact_handoff_status(artifact)),
            pool: Some(artifact.pool.clone()),
            title: Some(format!("{} ({})", artifact.title, artifact.artifact_id)),
            content: artifact_content(room, artifact, brehon_root),
            created_at: Some(artifact.created_at),
        });
    }
    events.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.kind.cmp(&right.kind))
    });
    for (idx, event) in events.iter_mut().enumerate() {
        event.seq = idx + 1;
    }
    events
}

fn research_event_block_lines(event: &ResearchEvent, width: usize) -> Vec<Line<'static>> {
    if event.role == "request" {
        return research_request_block_lines(event, width);
    }

    research_artifact_block_lines(event, width)
}

fn research_artifact_block_lines(event: &ResearchEvent, width: usize) -> Vec<Line<'static>> {
    let time = event
        .created_at
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default();
    let status = event.status.as_deref().unwrap_or("attached");
    let status_style = Style::default()
        .fg(artifact_status_color(status))
        .add_modifier(Modifier::BOLD);
    let role = if event.role.trim().is_empty() {
        "artifact".to_string()
    } else {
        event.role.clone()
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(
            truncate_to(&format!("#{:03} ", event.seq), width),
            Style::default().fg(TEXT_DIM),
        ),
        Span::styled(truncate_to(status, 18), status_style),
        Span::styled(" artifact/", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            truncate_to(&role, width.saturating_sub(36)),
            Style::default().fg(crate::theme::brand::PRIMARY),
        ),
        Span::styled("  by ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            truncate_to(&event.author, 18),
            Style::default().fg(chrome::TEXT_BODY),
        ),
        Span::styled(format!(" {time}"), Style::default().fg(TEXT_MUTED)),
    ])];

    if let Some(title) = event
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        lines.push(Line::from(Span::styled(
            truncate_to(&format!("  {title}"), width),
            Style::default()
                .fg(chrome::TEXT_BODY)
                .add_modifier(Modifier::BOLD),
        )));
    }

    let parsed = parse_artifact_content(&event.content);
    for summary in wrap_text_lines(&parsed.summary, width.saturating_sub(4), 2) {
        lines.push(Line::from(Span::styled(
            format!("    {summary}"),
            Style::default().fg(chrome::TEXT_SOFT),
        )));
    }

    let mut meta = Vec::new();
    if let Some(pool) = event.pool.as_deref().filter(|pool| !pool.trim().is_empty()) {
        meta.push(format!("pool {pool}"));
    }
    meta.extend(parsed.meta.iter().cloned());
    if !meta.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_to(&format!("    {}", meta.join(" / ")), width),
            Style::default().fg(TEXT_DIM),
        )));
    }

    for line in parsed.paths.iter().take(2) {
        lines.push(Line::from(Span::styled(
            truncate_to(&format!("    {line}"), width),
            Style::default().fg(TEXT_MUTED),
        )));
    }
    for line in parsed.delivery.iter().take(3) {
        lines.push(Line::from(Span::styled(
            truncate_to(&format!("    {line}"), width),
            if line.starts_with("Warnings:") {
                Style::default().fg(crate::theme::status::WARNING)
            } else {
                Style::default().fg(TEXT_DIM)
            },
        )));
    }

    if lines.len() == 1 {
        lines.push(Line::from(Span::styled(
            "    (empty research artifact)",
            Style::default().fg(TEXT_MUTED),
        )));
    }
    lines
}

#[derive(Default)]
struct ParsedArtifactContent {
    summary: String,
    meta: Vec<String>,
    paths: Vec<String>,
    delivery: Vec<String>,
}

fn parse_artifact_content(content: &str) -> ParsedArtifactContent {
    let mut parsed = ParsedArtifactContent::default();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("## ") {
            continue;
        }
        if let Some(value) = line.strip_prefix("Confidence:") {
            parsed.meta.push(format!("confidence {}", value.trim()));
        } else if let Some(value) = line.strip_prefix("Citations:") {
            parsed.meta.push(format!("citations {}", value.trim()));
        } else if let Some(value) = line.strip_prefix("Supersedes:") {
            parsed.meta.push(format!("supersedes {}", value.trim()));
        } else if let Some(value) = line.strip_prefix("Brief:") {
            parsed.paths.push(format!("brief {}", value.trim()));
        } else if let Some(value) = line.strip_prefix("Data:") {
            parsed.paths.push(format!("data {}", value.trim()));
        } else if let Some(value) = line.strip_prefix("Handoff:") {
            for delivery in value
                .split(';')
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                parsed.delivery.push(format!("handoff {delivery}"));
            }
        } else if let Some(value) = line.strip_prefix("Warnings:") {
            parsed.delivery.push(format!("Warnings: {}", value.trim()));
        } else if parsed.summary.is_empty() {
            parsed.summary = line.to_string();
        } else {
            parsed.summary.push(' ');
            parsed.summary.push_str(line);
        }
    }
    parsed
}

fn wrap_text_lines(value: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        let next_width = if current.is_empty() {
            word.width()
        } else {
            current.width() + 1 + word.width()
        };
        if !current.is_empty() && next_width > width {
            lines.push(current);
            current = String::new();
            if lines.len() == max_lines {
                break;
            }
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    lines
        .into_iter()
        .map(|line| truncate_to(&line, width))
        .collect()
}

fn artifact_status_color(status: &str) -> Color {
    if status.contains("warning") || status.contains("failed") {
        crate::theme::status::WARNING
    } else if status.contains("queued") || status.contains("running") {
        crate::theme::brand::PRIMARY
    } else {
        crate::theme::status::SUCCESS
    }
}

fn research_request_block_lines(event: &ResearchEvent, width: usize) -> Vec<Line<'static>> {
    let time = event
        .created_at
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default();
    let status = event.status.as_deref().unwrap_or("queued");
    let status_color = match status {
        JOB_STATUS_RUNNING => crate::theme::status::WARNING,
        JOB_STATUS_COMPLETED => crate::theme::status::SUCCESS,
        _ => TEXT_MUTED,
    };
    let pool = event.pool.as_deref().unwrap_or("pool");
    let title = event.title.as_deref().unwrap_or("request");
    let mut lines = vec![Line::from(vec![
        Span::styled(
            truncate_to(&format!("#{:03} ", event.seq), width),
            Style::default().fg(TEXT_DIM),
        ),
        Span::styled(
            truncate_to(status, 12),
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" request ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            truncate_to(title, width.saturating_sub(38)),
            Style::default().fg(chrome::TEXT_BODY),
        ),
        Span::styled(format!(" {time}"), Style::default().fg(TEXT_MUTED)),
    ])];
    lines.push(Line::from(vec![
        Span::styled("     pool ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            truncate_to(pool, width.saturating_sub(11)),
            Style::default().fg(TEXT_MUTED),
        ),
    ]));
    for preview in event.content.lines().take(3) {
        let preview = preview.trim();
        if preview.is_empty() {
            continue;
        }
        lines.push(Line::from(Span::styled(
            truncate_to(&format!("     {preview}"), width),
            Style::default().fg(chrome::TEXT_SOFT),
        )));
    }
    lines
}

fn artifact_handoff_status(artifact: &ResearchArtifactFile) -> String {
    if artifact.handoff_deliveries.is_empty() {
        return "attached".to_string();
    }
    if artifact
        .handoff_deliveries
        .iter()
        .any(|delivery| delivery.status == "failed")
    {
        return "handoff warning".to_string();
    }
    if artifact
        .handoff_deliveries
        .iter()
        .any(|delivery| delivery.status == "queued")
    {
        return "handoff queued".to_string();
    }
    "attached".to_string()
}

fn artifact_content(
    _room: &ResearchRoomFile,
    artifact: &ResearchArtifactFile,
    brehon_root: Option<&Path>,
) -> String {
    let mut content = format!("## {}\n{}", artifact.title, artifact.summary);
    if let Some(confidence) = artifact.confidence.as_deref() {
        content.push_str(&format!("\nConfidence: {confidence}"));
    }
    if !artifact.citations.is_empty() {
        let first = artifact
            .citations
            .first()
            .map(String::as_str)
            .unwrap_or_default();
        content.push_str(&format!(
            "\nCitations: {}{}",
            artifact.citations.len(),
            if first.is_empty() {
                String::new()
            } else {
                format!(" ({first})")
            }
        ));
    }
    if !artifact.supersedes.is_empty() {
        content.push_str("\nSupersedes: ");
        content.push_str(&artifact.supersedes.join(", "));
    }
    if let Some(path) = resolve_artifact_path(brehon_root, &artifact.artifact_path) {
        content.push_str(&format!(
            "\nBrief: {}",
            display_artifact_path(brehon_root, &path)
        ));
    } else if !artifact.artifact_path.is_empty() {
        content.push_str(&format!("\nBrief: {}", artifact.artifact_path));
    }
    if !artifact.structured_path.is_empty() {
        content.push_str(&format!("\nData: {}", artifact.structured_path));
    }
    if !artifact.handoff_deliveries.is_empty() {
        let deliveries = artifact
            .handoff_deliveries
            .iter()
            .map(|delivery| {
                let target = if delivery.target.trim().is_empty() {
                    "unknown"
                } else {
                    delivery.target.as_str()
                };
                let role = if delivery.target_role.trim().is_empty() {
                    "target"
                } else {
                    delivery.target_role.as_str()
                };
                let method = delivery
                    .method
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!(" via {value}"))
                    .unwrap_or_default();
                let prompt_id = delivery
                    .prompt_id
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!(" #{value}"))
                    .unwrap_or_default();
                format!("{role} {target}: {}{method}{prompt_id}", delivery.status)
            })
            .collect::<Vec<_>>()
            .join("; ");
        content.push_str(&format!("\nHandoff: {deliveries}"));
    }
    if !artifact.handoff_warnings.is_empty() {
        content.push_str("\nWarnings: ");
        content.push_str(&artifact.handoff_warnings.join("; "));
    } else if let Some(warning) = artifact
        .handoff_deliveries
        .iter()
        .find_map(|delivery| delivery.warning.as_deref())
    {
        content.push_str("\nWarnings: ");
        content.push_str(warning);
    }
    content
}

fn request_preview(job: &ResearchJobFile) -> String {
    let first_line = job
        .prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("No prompt text.");
    let acceptance = prompt_section_first_line(&job.prompt, "Acceptance Criteria:")
        .or_else(|| prompt_section_first_line(&job.prompt, "Test Requirements:"));
    let hints = prompt_section_first_line(&job.prompt, "File Hints:");
    let mut lines = vec![truncate_to(first_line, 180)];
    if let Some(acceptance) = acceptance {
        lines.push(format!("Acceptance: {}", truncate_to(&acceptance, 150)));
    }
    if let Some(hint) = hints {
        lines.push(format!("Hint: {}", truncate_to(&hint, 150)));
    }
    lines.join("\n")
}

fn prompt_section_first_line(prompt: &str, heading: &str) -> Option<String> {
    let mut in_section = false;
    let heading = heading.to_ascii_lowercase();
    for line in prompt.lines() {
        let trimmed = line.trim();
        if trimmed.to_ascii_lowercase() == heading {
            in_section = true;
            continue;
        }
        if in_section {
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.ends_with(':') && !trimmed.starts_with('-') {
                return None;
            }
            return Some(trimmed.trim_start_matches("- ").to_string());
        }
    }
    None
}

fn display_artifact_path(brehon_root: Option<&Path>, path: &Path) -> String {
    brehon_root
        .and_then(|root| path.strip_prefix(root).ok())
        .map(|path| format!(".brehon/{}", path.display()))
        .unwrap_or_else(|| path.display().to_string())
}

fn read_research_rooms(
    brehon_root: Option<&Path>,
    loader: &ProjectConfigLoader,
) -> Vec<ResearchRoomFile> {
    read_research_rooms_matching(brehon_root, loader, None)
}

fn read_research_rooms_for_render(
    brehon_root: Option<&Path>,
    loader: &ProjectConfigLoader,
    selected_task_id: Option<&str>,
) -> Vec<ResearchRoomFile> {
    read_research_rooms_matching(brehon_root, loader, selected_task_id)
}

fn read_research_rooms_matching(
    brehon_root: Option<&Path>,
    loader: &ProjectConfigLoader,
    selected_task_id: Option<&str>,
) -> Vec<ResearchRoomFile> {
    let Some(brehon_root) = brehon_root else {
        return Vec::new();
    };
    let root = research_root_for_reading(brehon_root, loader);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut rooms = Vec::new();
    for entry in entries.flatten().take(256) {
        if entry.file_name().to_string_lossy().starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        let task_id = entry.file_name().to_string_lossy().to_string();
        let jobs = read_jobs(entry.path().join("jobs"));
        let manifest = read_manifest(entry.path().join("manifest.yaml"));
        let artifacts = manifest
            .as_ref()
            .map(|manifest| manifest.artifacts.clone())
            .unwrap_or_default();
        if jobs.is_empty() && artifacts.is_empty() {
            continue;
        }
        let task_id = manifest
            .as_ref()
            .map(|manifest| manifest.task_id.clone())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(task_id);
        let task = read_task(brehon_root, &task_id);
        if task
            .as_ref()
            .and_then(task_status_from_task)
            .is_some_and(is_terminal_task_status)
            && selected_task_id != Some(task_id.as_str())
        {
            continue;
        }
        let updated_at = jobs
            .iter()
            .map(|job| job.updated_at)
            .chain(manifest.as_ref().and_then(|manifest| manifest.updated_at))
            .chain(artifacts.iter().map(|artifact| artifact.created_at))
            .max();
        rooms.push(ResearchRoomFile {
            title: task.as_ref().and_then(task_title_from_task),
            task_id,
            jobs,
            artifacts,
            updated_at,
        });
    }
    rooms.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.task_id.cmp(&right.task_id))
    });
    rooms
}

fn read_jobs(jobs_dir: PathBuf) -> Vec<ResearchJobFile> {
    let Ok(entries) = std::fs::read_dir(jobs_dir) else {
        return Vec::new();
    };
    let mut jobs = Vec::new();
    for entry in entries.flatten().take(512) {
        if entry.file_name().to_string_lossy().starts_with('.')
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("json")
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Ok(job) = serde_json::from_str::<ResearchJobFile>(&content) {
            jobs.push(job);
        }
    }
    jobs.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.job_id.cmp(&right.job_id))
    });
    jobs
}

fn read_manifest(path: PathBuf) -> Option<ResearchManifestFile> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&content).ok()
}

fn parse_operator_request(
    default_task_id: Option<&str>,
    content: &str,
) -> Result<(String, String), String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("Type a research request before sending.".to_string());
    }

    if let Some(rest) = trimmed.strip_prefix("/task ") {
        let rest = rest.trim_start();
        let Some((task_id, prompt)) = split_first_token(rest) else {
            return Err("Use `/task T-123 <research request>`.".to_string());
        };
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("Use `/task T-123 <research request>`.".to_string());
        }
        return Ok((task_id.to_string(), prompt.to_string()));
    }

    if default_task_id.is_none() {
        if let Some((task_id, prompt)) = split_task_prefix(trimmed) {
            return Ok((task_id.to_string(), prompt.trim().to_string()));
        }
    }

    let Some(task_id) = default_task_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err("No research room is selected. Send `/task T-123 <request>`.".to_string());
    };
    Ok((task_id.to_string(), trimmed.to_string()))
}

fn split_first_token(value: &str) -> Option<(&str, &str)> {
    let token_end = value
        .char_indices()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(idx))?;
    Some((&value[..token_end], &value[token_end..]))
}

fn split_task_prefix(value: &str) -> Option<(&str, &str)> {
    let (task_id, prompt) = value.split_once(':')?;
    let task_id = task_id.trim();
    let prompt = prompt.trim();
    if task_id.is_empty() || prompt.is_empty() || task_id.contains(' ') {
        return None;
    }
    Some((task_id, prompt))
}

fn notify_research_agents(
    brehon_root: &Path,
    session_name: Option<&str>,
    pool: &ResearchPoolConfig,
    job: &ResearchJobFile,
) -> Vec<String> {
    let sessions_dir = brehon_root.join("runtime").join("sessions");
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut notified = Vec::new();
    let message = format!(
        "Research room request queued for task {}.\n\
job_id: {}\n\
pool: {}\n\
role: {}\n\n\
Prompt:\n{}\n\n\
Claim it with `research action=claim_next pool={}`. Submit with `research action=submit task_id={} job_id={} summary=\"...\" content=\"...\" citations='[...]'`.",
        job.task_id, job.job_id, job.pool, job.role, job.prompt, job.pool, job.task_id, job.job_id
    );
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.')
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("json")
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        if session.get("role").and_then(Value::as_str) != Some("research") {
            continue;
        }
        if !session_is_live(&session) || !session_matches_runtime(&session, session_name) {
            continue;
        }
        let agent_type = session
            .get("agent_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !agent_type.is_empty() && agent_type != pool.lane && agent_type != pool.id {
            continue;
        }
        let Some(name) = session.get("name").and_then(Value::as_str) else {
            continue;
        };
        if enqueue_composer_message(brehon_root, session_name, name, &message).is_ok() {
            notified.push(name.to_string());
        }
    }
    notified
}

fn session_is_live(entry: &Value) -> bool {
    let timestamp = entry
        .get("last_seen_at")
        .and_then(Value::as_str)
        .or_else(|| entry.get("registered_at").and_then(Value::as_str))
        .or_else(|| entry.get("started_at").and_then(Value::as_str));
    let Some(timestamp) = timestamp else {
        return true;
    };
    let Ok(parsed) = DateTime::parse_from_rfc3339(timestamp) else {
        return true;
    };
    Utc::now().signed_duration_since(parsed.with_timezone(&Utc)) <= chrono::Duration::minutes(15)
}

fn session_matches_runtime(entry: &Value, session_name: Option<&str>) -> bool {
    let Some(expected) = session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };
    entry
        .get("session_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        == Some(expected)
}

fn write_job(path: &Path, job: &ResearchJobFile) -> Result<(), String> {
    let payload = serde_json::to_vec_pretty(job)
        .map_err(|err| format!("failed to serialize research job: {err}"))?;
    atomic_write(path, &payload)
}

fn next_job_id(jobs_dir: &Path, task_id: &str, template_id: &str) -> Result<String, String> {
    let prefix = format!("RJOB-{}-{}", sanitize_id(task_id), sanitize_id(template_id));
    let seq = next_sequence(jobs_dir, &prefix)?;
    Ok(format!("{prefix}-{seq:03}"))
}

fn next_sequence(dir: &Path, prefix: &str) -> Result<u32, String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("failed to create research dir {}: {err}", dir.display()))?;
    let mut max_seq = 0u32;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(1);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(prefix) {
            continue;
        }
        let name = name.strip_suffix(".json").unwrap_or(&name);
        if let Some(raw) = name.rsplit('-').next() {
            if let Ok(seq) = raw.parse::<u32>() {
                max_seq = max_seq.max(seq);
            }
        }
    }
    Ok(max_seq + 1)
}

fn atomic_write(path: &Path, payload: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create dir {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp-{}",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp"),
        std::process::id()
    ));
    std::fs::write(&tmp, payload)
        .map_err(|err| format!("failed to write temp file {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("failed to install {}: {err}", path.display()))
}

fn research_root_for_reading(brehon_root: &Path, loader: &ProjectConfigLoader) -> PathBuf {
    loader(brehon_root)
        .and_then(|config| research_root_from_config(brehon_root, &config.research).ok())
        .unwrap_or_else(|| brehon_root.join("runtime").join("research"))
}

fn research_root_from_config(
    brehon_root: &Path,
    config: &ResearchConfig,
) -> Result<PathBuf, String> {
    let project_root = project_root_from_brehon_root(brehon_root);
    let path = PathBuf::from(&config.artifact_root);
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(project_root.join(path))
}

fn project_root_from_brehon_root(brehon_root: &Path) -> PathBuf {
    if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        return brehon_root.parent().unwrap_or(brehon_root).to_path_buf();
    }
    brehon_root.to_path_buf()
}

fn read_task(brehon_root: &Path, task_id: &str) -> Option<Value> {
    let path = brehon_root
        .join("runtime")
        .join("tasks")
        .join(format!("{}.json", sanitize_id(task_id)));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_task_title(brehon_root: &Path, task_id: &str) -> Option<String> {
    read_task(brehon_root, task_id).and_then(|task| task_title_from_task(&task))
}

fn task_title_from_task(task: &Value) -> Option<String> {
    task.get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn task_status_from_task(task: &Value) -> Option<&str> {
    task.get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn task_id_from_task(task: &Value) -> Option<&str> {
    task.get("task_id")
        .and_then(Value::as_str)
        .or_else(|| task.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn resolve_artifact_path(brehon_root: Option<&Path>, artifact_path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(artifact_path);
    if path.is_absolute() {
        return Some(path);
    }
    let project_root = brehon_root.map(project_root_from_brehon_root)?;
    Some(project_root.join(path))
}

fn sanitize_id(value: &str) -> String {
    value
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    /// Build a `ProjectConfigLoader` that calls into `brehon-config` exactly
    /// the way `brehon-cli` will at runtime. Kept in the test module so the
    /// production crate doesn't pick up a `brehon-config` dependency.
    fn test_loader() -> ProjectConfigLoader {
        Arc::new(|brehon_root: &Path| {
            let project_root =
                if brehon_root.file_name().and_then(|n| n.to_str()) == Some(".brehon") {
                    brehon_root.parent().unwrap_or(brehon_root)
                } else {
                    brehon_root
                };
            brehon_config::load_config(Some(project_root)).ok()
        })
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let mut rows = Vec::new();
        for row in 0..buffer.area.height {
            let text = (0..buffer.area.width)
                .filter_map(|col| buffer.cell((col, row)).map(|cell| cell.symbol()))
                .collect::<String>();
            rows.push(text);
        }
        rows.join("\n")
    }

    fn write_config(project: &Path) {
        std::fs::create_dir_all(project.join(".brehon/runtime/tasks")).unwrap();
        std::fs::write(
            project.join(".brehon/config.yaml"),
            r#"
research:
  enabled: true
  pools:
    - id: spec-research
      lane: codex-worker
      role: normative_requirements
      min: 0
      max: 1
      cost_units: 1
"#,
        )
        .unwrap();
    }

    fn write_task(brehon_root: &Path, task_id: &str) {
        write_task_with_status(brehon_root, task_id, "Protocol parser", "open");
    }

    fn write_task_with_status(brehon_root: &Path, task_id: &str, title: &str, status: &str) {
        std::fs::create_dir_all(brehon_root.join("runtime/tasks")).unwrap();
        std::fs::write(
            brehon_root
                .join("runtime/tasks")
                .join(format!("{task_id}.json")),
            serde_json::json!({
                "task_id": task_id,
                "title": title,
                "status": status,
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();
    }

    fn write_queued_job(brehon_root: &Path, task_id: &str, prompt: &str) {
        let jobs_dir = brehon_root
            .join("runtime/research")
            .join(task_id)
            .join("jobs");
        std::fs::create_dir_all(&jobs_dir).unwrap();
        let now = Utc::now();
        let job_id = format!("RJOB-{task_id}-spec-001");
        let job = ResearchJobFile {
            job_id: job_id.clone(),
            task_id: task_id.to_string(),
            route_id: None,
            template_id: "spec".to_string(),
            pool: "spec-research".to_string(),
            lane: "codex-worker".to_string(),
            role: "normative_requirements".to_string(),
            status: JOB_STATUS_QUEUED.to_string(),
            origin: "manual_request".to_string(),
            prompt: prompt.to_string(),
            cost_units: 1,
            requested_by: "operator".to_string(),
            assigned_to: None,
            artifact_id: None,
            depends_on: Vec::new(),
            warnings: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        write_job(&jobs_dir.join(format!("{job_id}.json")), &job).unwrap();
    }

    fn write_session(brehon_root: &Path, name: &str, role: &str) {
        let sessions_dir = brehon_root.join("runtime/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let now = Utc::now().to_rfc3339();
        std::fs::write(
            sessions_dir.join(format!("{name}.json")),
            serde_json::json!({
                "name": name,
                "role": role,
                "session_id": format!("session-{name}"),
                "registered_at": now,
                "last_seen_at": now
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn post_operator_request_writes_room_job() {
        let temp = tempfile::tempdir().unwrap();
        write_config(temp.path());
        let brehon_root = temp.path().join(".brehon");
        write_task(&brehon_root, "T-1");

        let loader = test_loader();
        let result = post_operator_research_request(
            &brehon_root,
            &loader,
            Some("T-1"),
            "Find the relevant RFC sections.",
            Some("brehon-test"),
        )
        .unwrap();

        assert_eq!(result.task_id, "T-1");
        assert_eq!(research_room_count(Some(&brehon_root), &loader), 1);
        let rooms = read_research_rooms(Some(&brehon_root), &loader);
        assert_eq!(rooms[0].jobs[0].status, JOB_STATUS_QUEUED);
        assert!(rooms[0].jobs[0].prompt.contains("RFC sections"));
    }

    #[test]
    fn research_room_count_ignores_terminal_tasks_by_default() {
        let temp = tempfile::tempdir().unwrap();
        write_config(temp.path());
        let brehon_root = temp.path().join(".brehon");
        write_task_with_status(&brehon_root, "T-active", "Active protocol", "in_progress");
        write_task_with_status(&brehon_root, "T-closed", "Closed protocol", "closed");
        write_queued_job(&brehon_root, "T-active", "active research prompt");
        write_queued_job(&brehon_root, "T-closed", "closed research prompt");

        let loader = test_loader();
        assert_eq!(research_room_count(Some(&brehon_root), &loader), 1);
        assert_eq!(
            active_research_room_task_id(Some(&brehon_root), &loader).as_deref(),
            Some("T-active")
        );

        let rooms = read_research_rooms_for_render(Some(&brehon_root), &loader, Some("T-closed"));
        assert!(rooms.iter().any(|room| room.task_id == "T-closed"));

        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        let mut state = ResearchRoomViewState {
            selected_task_id: Some("T-closed".to_string()),
            ..ResearchRoomViewState::default()
        };
        terminal
            .draw(|frame| {
                render_research_view(frame, frame.area(), Some(&brehon_root), &loader, &mut state)
            })
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Closed protocol"));
        assert!(rendered.contains("closed research prompt"));
    }

    #[test]
    fn render_research_header_distinguishes_live_agents_from_rooms() {
        let temp = tempfile::tempdir().unwrap();
        write_config(temp.path());
        let brehon_root = temp.path().join(".brehon");
        write_task_with_status(&brehon_root, "T-active", "Active protocol", "in_progress");
        write_queued_job(&brehon_root, "T-active", "active research prompt");
        write_session(&brehon_root, "research-1", "research");
        write_session(&brehon_root, "worker-1", "worker");

        let loader = test_loader();
        assert_eq!(live_research_agent_count(Some(&brehon_root)), 1);

        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        let mut state = ResearchRoomViewState::default();
        terminal
            .draw(|frame| {
                render_research_view(frame, frame.area(), Some(&brehon_root), &loader, &mut state)
            })
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("1 agents / 1 rooms / 1 queued / 0 running / 0 done"));
    }

    #[test]
    fn post_operator_request_accepts_task_prefix_without_room() {
        let temp = tempfile::tempdir().unwrap();
        write_config(temp.path());
        let brehon_root = temp.path().join(".brehon");
        write_task(&brehon_root, "T-2");

        let result = post_operator_research_request(
            &brehon_root,
            &test_loader(),
            None,
            "/task T-2 Map the code paths.",
            Some("brehon-test"),
        )
        .unwrap();

        assert_eq!(result.task_id, "T-2");
        assert!(result.job_id.starts_with("RJOB-T-2-operator-request"));
    }

    #[test]
    fn render_research_room_shows_compact_artifact_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(brehon_root.join("runtime/research/T-3/jobs")).unwrap();
        write_task(&brehon_root, "T-3");
        let now = Utc::now();
        let job = ResearchJobFile {
            job_id: "RJOB-T-3-spec-001".to_string(),
            task_id: "T-3".to_string(),
            route_id: None,
            template_id: "spec".to_string(),
            pool: "spec-research".to_string(),
            lane: "codex-worker".to_string(),
            role: "normative_requirements".to_string(),
            status: JOB_STATUS_COMPLETED.to_string(),
            origin: "manual_request".to_string(),
            prompt: "Summarize the protocol requirements with citations.".to_string(),
            cost_units: 1,
            requested_by: "operator".to_string(),
            assigned_to: Some("researcher-1".to_string()),
            artifact_id: Some("RCH-T-3-spec-001".to_string()),
            depends_on: Vec::new(),
            warnings: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        write_job(
            &brehon_root.join("runtime/research/T-3/jobs/RJOB-T-3-spec-001.json"),
            &job,
        )
        .unwrap();
        std::fs::write(
            brehon_root.join("runtime/research/T-3/manifest.yaml"),
            serde_yaml::to_string(&serde_json::json!({
                "task_id": "T-3",
                "updated_at": now,
                "artifacts": [{
                    "artifact_id": "RCH-T-3-spec-001",
                    "job_id": "RJOB-T-3-spec-001",
                    "pool": "spec-research",
                    "role": "normative_requirements",
                    "title": "Protocol Requirements",
                    "summary": "The protocol requires strict ordering.",
                    "artifact_path": ".brehon/runtime/research/T-3/RCH-T-3-spec-001/brief.md",
                    "structured_path": ".brehon/runtime/research/T-3/RCH-T-3-spec-001/artifact.yaml",
                    "citations": ["RFC 9999 section 1"],
                    "handoff_deliveries": [
                        {
                            "target": "worker-1",
                            "target_role": "worker",
                            "status": "queued",
                            "method": "queued",
                            "prompt_id": "prompt-worker-1"
                        },
                        {
                            "target": "supervisor-1",
                            "target_role": "supervisor",
                            "status": "failed",
                            "warning": "research handoff could not notify supervisor"
                        }
                    ],
                    "handoff_warnings": ["research handoff could not notify supervisor"],
                    "created_at": now
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
        let mut state = ResearchRoomViewState::default();
        terminal
            .draw(|frame| {
                render_research_view(
                    frame,
                    frame.area(),
                    Some(&brehon_root),
                    &test_loader(),
                    &mut state,
                )
            })
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Research"));
        assert!(rendered.contains("Protocol parser"));
        assert!(
            !rendered.contains("Summarize the protocol requirements"),
            "completed request rows with artifacts should collapse out of the detail view"
        );
        assert!(rendered.contains("Protocol Requirements"));
        assert!(rendered.contains("The protocol requires strict ordering."));
        assert!(rendered.contains("handoff warning"));
        assert!(rendered.contains("worker worker-1: queued"));
        assert!(rendered.contains("supervisor-1"));
        assert!(rendered.contains("failed"));
        assert!(rendered.contains("Warnings: research handoff could not notify supervisor"));
    }

    #[test]
    fn render_research_room_can_focus_task_without_existing_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        write_task(&brehon_root, "T-empty");
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        let mut state = ResearchRoomViewState {
            selected_task_id: Some("T-empty".to_string()),
            ..ResearchRoomViewState::default()
        };

        terminal
            .draw(|frame| {
                render_research_view(
                    frame,
                    frame.area(),
                    Some(&brehon_root),
                    &test_loader(),
                    &mut state,
                )
            })
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Protocol parser"));
        assert!(rendered.contains("No requests or artifacts"));
        assert_eq!(state.selected_task_id.as_deref(), Some("T-empty"));
    }
}
