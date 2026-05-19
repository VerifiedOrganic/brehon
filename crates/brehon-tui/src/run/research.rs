//! Research room rendering and operator request enqueueing.

use std::path::{Path, PathBuf};

use brehon_types::{BrehonConfig, ResearchConfig, ResearchPoolConfig};
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

use super::advisors::{
    advisor_author_color, bubble_line, markdown_bubble_lines, message_bubble_width,
};
use super::composer::enqueue_composer_message;
use super::rendering::truncate_to;
use super::types::ResearchRoomViewState;

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
    created_at: DateTime<Utc>,
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

fn default_job_status() -> String {
    JOB_STATUS_QUEUED.to_string()
}

fn default_job_origin() -> String {
    "manual_request".to_string()
}

fn default_requested_by() -> String {
    "operator".to_string()
}

pub(crate) fn research_room_count(brehon_root: Option<&Path>) -> usize {
    read_research_rooms(brehon_root).len()
}

pub(crate) fn active_research_room_task_id(brehon_root: Option<&Path>) -> Option<String> {
    read_research_rooms(brehon_root)
        .into_iter()
        .next()
        .map(|room| room.task_id)
}

pub(crate) fn post_operator_research_request(
    brehon_root: &Path,
    default_task_id: Option<&str>,
    content: &str,
    session_name: Option<&str>,
) -> Result<ResearchPostResult, String> {
    let config = load_project_config(brehon_root)?;
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
    state: &mut ResearchRoomViewState,
) {
    let mut rooms = read_research_rooms(brehon_root);
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
    let inner = Panel::new("Research")
        .subtitle("async evidence rooms")
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
        let lines = empty_state_lines(brehon_root, inner.width);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let columns = if inner.width > 86 {
        Layout::horizontal([Constraint::Length(34), Constraint::Min(24)]).split(inner)
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

fn empty_state_lines(brehon_root: Option<&Path>, width: u16) -> Vec<Line<'static>> {
    let config_hint = match brehon_root.and_then(|root| load_project_config(root).ok()) {
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
    for (idx, room) in rooms.iter().take(area.height as usize / 3 + 1).enumerate() {
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
            lines.push(Line::from(Span::styled(
                truncate_to(&format!("  {}", room.task_id), area.width as usize),
                Style::default().fg(TEXT_DIM),
            )));
        }
        if lines.len() < area.height as usize {
            lines.push(Line::from(status_spans(room, area.width as usize)));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn status_spans(room: &ResearchRoomFile, width: usize) -> Vec<Span<'static>> {
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
    let text = truncate_to(
        &format!(
            "  {queued} queued / {running} running / {completed} done / {} artifacts",
            room.artifacts.len()
        ),
        width,
    );
    vec![Span::styled(text, Style::default().fg(TEXT_MUTED))]
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
            content: job.prompt.clone(),
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
            status: Some("attached".to_string()),
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
    let bubble_width = message_bubble_width(width);
    let author_color = research_author_color(event);
    let bubble_bg = if event.role == "request" {
        chrome::BG_ELEVATED
    } else {
        chrome::PANEL_BG_ELEVATED
    };
    let bubble_style = Style::default().fg(chrome::TEXT_BODY).bg(bubble_bg);
    let meta_style = Style::default().fg(TEXT_DIM).bg(bubble_bg);
    let time = event
        .created_at
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default();
    let role = if event.role.is_empty() {
        event.kind.clone()
    } else {
        format!("{}/{}", event.kind, event.role)
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(
            truncate_to(
                &format!("#{:03} ", event.seq),
                width.saturating_sub(time.width() + 1),
            ),
            Style::default().fg(TEXT_DIM),
        ),
        Span::styled(
            truncate_to(
                &format!("{} ({role})", event.author),
                width.saturating_sub(time.width() + 6),
            ),
            Style::default()
                .fg(author_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {time}"), Style::default().fg(TEXT_MUTED)),
    ])];

    let mut meta = Vec::new();
    if let Some(status) = event.status.as_deref() {
        meta.push(format!("status: {status}"));
    }
    if let Some(pool) = event.pool.as_deref() {
        meta.push(format!("pool: {pool}"));
    }
    if let Some(title) = event.title.as_deref() {
        meta.push(format!("title: {title}"));
    }
    if !meta.is_empty() {
        lines.push(bubble_line(
            &meta.join(" / "),
            bubble_width,
            meta_style,
            author_color,
        ));
    }

    let mut body = markdown_bubble_lines(&event.content, bubble_width, bubble_style, author_color);
    if body.is_empty() {
        body.push(bubble_line(
            "(empty research event)",
            bubble_width,
            bubble_style,
            author_color,
        ));
    }
    lines.extend(body);
    lines
}

fn research_author_color(event: &ResearchEvent) -> Color {
    if event.role == "request" || event.author == "operator" {
        return crate::theme::brand::PRIMARY;
    }
    advisor_author_color(&event.author, &event.role)
}

fn artifact_content(
    room: &ResearchRoomFile,
    artifact: &ResearchArtifactFile,
    brehon_root: Option<&Path>,
) -> String {
    if let Some(path) = resolve_artifact_path(brehon_root, &artifact.artifact_path) {
        if let Ok(content) = std::fs::read_to_string(path) {
            return truncate_chars(&content, 12_000);
        }
    }
    let mut content = format!("## {}\n\n{}", artifact.title, artifact.summary);
    if let Some(confidence) = artifact.confidence.as_deref() {
        content.push_str(&format!("\n\nConfidence: {confidence}"));
    }
    if !artifact.citations.is_empty() {
        content.push_str("\n\n## Citations\n");
        for citation in &artifact.citations {
            content.push_str(&format!("- {citation}\n"));
        }
    }
    if !artifact.supersedes.is_empty() {
        content.push_str("\nSupersedes: ");
        content.push_str(&artifact.supersedes.join(", "));
    }
    if !artifact.structured_path.is_empty() {
        content.push_str(&format!(
            "\nStructured artifact: {}",
            artifact.structured_path
        ));
    }
    if !room.task_id.is_empty() {
        content.push_str(&format!("\nTask: {}", room.task_id));
    }
    content
}

fn read_research_rooms(brehon_root: Option<&Path>) -> Vec<ResearchRoomFile> {
    let Some(brehon_root) = brehon_root else {
        return Vec::new();
    };
    let root = research_root_for_reading(brehon_root);
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
        let updated_at = jobs
            .iter()
            .map(|job| job.updated_at)
            .chain(manifest.as_ref().and_then(|manifest| manifest.updated_at))
            .chain(artifacts.iter().map(|artifact| artifact.created_at))
            .max();
        rooms.push(ResearchRoomFile {
            title: read_task_title(brehon_root, &task_id),
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

fn research_root_for_reading(brehon_root: &Path) -> PathBuf {
    load_project_config(brehon_root)
        .ok()
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

fn load_project_config(brehon_root: &Path) -> Result<BrehonConfig, String> {
    let project_root = project_root_from_brehon_root(brehon_root);
    brehon_config::load_config(Some(&project_root)).map_err(|err| {
        format!(
            "failed to load project config from {}: {err}",
            project_root.display()
        )
    })
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
    read_task(brehon_root, task_id).and_then(|task| {
        task.get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
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

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

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
        std::fs::create_dir_all(brehon_root.join("runtime/tasks")).unwrap();
        std::fs::write(
            brehon_root
                .join("runtime/tasks")
                .join(format!("{task_id}.json")),
            serde_json::json!({
                "task_id": task_id,
                "title": "Protocol parser",
                "status": "open",
                "task_type": "task"
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

        let result = post_operator_research_request(
            &brehon_root,
            Some("T-1"),
            "Find the relevant RFC sections.",
            Some("brehon-test"),
        )
        .unwrap();

        assert_eq!(result.task_id, "T-1");
        assert_eq!(research_room_count(Some(&brehon_root)), 1);
        let rooms = read_research_rooms(Some(&brehon_root));
        assert_eq!(rooms[0].jobs[0].status, JOB_STATUS_QUEUED);
        assert!(rooms[0].jobs[0].prompt.contains("RFC sections"));
    }

    #[test]
    fn post_operator_request_accepts_task_prefix_without_room() {
        let temp = tempfile::tempdir().unwrap();
        write_config(temp.path());
        let brehon_root = temp.path().join(".brehon");
        write_task(&brehon_root, "T-2");

        let result = post_operator_research_request(
            &brehon_root,
            None,
            "/task T-2 Map the code paths.",
            Some("brehon-test"),
        )
        .unwrap();

        assert_eq!(result.task_id, "T-2");
        assert!(result.job_id.starts_with("RJOB-T-2-operator-request"));
    }

    #[test]
    fn render_research_room_shows_request_and_artifact_blocks() {
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
                    "created_at": now
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut terminal = Terminal::new(TestBackend::new(100, 22)).unwrap();
        let mut state = ResearchRoomViewState::default();
        terminal
            .draw(|frame| render_research_view(frame, frame.area(), Some(&brehon_root), &mut state))
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Research"));
        assert!(rendered.contains("Protocol parser"));
        assert!(rendered.contains("Summarize the protocol requirements"));
        assert!(rendered.contains("Protocol Requirements"));
        assert!(rendered.contains("The protocol requires strict ordering."));
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
            .draw(|frame| render_research_view(frame, frame.area(), Some(&brehon_root), &mut state))
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Protocol parser"));
        assert!(rendered.contains("No requests or artifacts"));
        assert_eq!(state.selected_task_id.as_deref(), Some("T-empty"));
    }
}
