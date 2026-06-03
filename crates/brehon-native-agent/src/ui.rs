use brehon_adapter_sdk::JsonRpcResponse;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::{json, Value};
use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

pub(crate) enum TerminalEvent {
    GatewayConnected,
    GatewayDisconnected {
        error: Option<String>,
    },
    PermissionRequest {
        request_id: String,
        params: Option<Value>,
        response_tx: oneshot::Sender<JsonRpcResponse>,
    },
    TerminalInput {
        terminal_id: String,
        data: Vec<u8>,
    },
    SessionUpdate(Value),
}

pub(crate) type TerminalEventSink = mpsc::UnboundedSender<TerminalEvent>;
pub(crate) type TerminalEventReceiver = mpsc::UnboundedReceiver<TerminalEvent>;

pub(crate) fn event_channel() -> (TerminalEventSink, TerminalEventReceiver) {
    mpsc::unbounded_channel()
}

#[derive(Debug, Clone)]
pub(crate) struct TerminalUiConfig {
    pub provider: String,
    pub model: String,
    pub socket_path: String,
}

struct TerminalUiState {
    provider: String,
    model: String,
    socket_path: String,
    gateway: String,
    status: String,
    chat: Vec<String>,
    tools: Vec<ToolRow>,
    permissions: Vec<PermissionRow>,
    progress: Vec<String>,
    input: String,
}

struct ToolRow {
    id: String,
    title: String,
    status: String,
}

struct PermissionRow {
    request_id: String,
    summary: String,
    status: String,
    response_tx: Option<oneshot::Sender<JsonRpcResponse>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionChoice {
    AllowOnce,
    AllowSession,
    Deny,
}

impl TerminalUiState {
    fn new(config: TerminalUiConfig) -> Self {
        Self {
            provider: config.provider,
            model: config.model,
            socket_path: config.socket_path,
            gateway: "waiting for Brehon gateway".to_string(),
            status: "idle".to_string(),
            chat: Vec::new(),
            tools: Vec::new(),
            permissions: Vec::new(),
            progress: Vec::new(),
            input: String::new(),
        }
    }

    fn apply_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::GatewayConnected => {
                self.gateway = "connected".to_string();
            }
            TerminalEvent::GatewayDisconnected { error } => {
                self.gateway = error
                    .map(|error| format!("disconnected: {error}"))
                    .unwrap_or_else(|| "disconnected".to_string());
            }
            TerminalEvent::PermissionRequest {
                request_id,
                params,
                response_tx,
            } => {
                self.permissions.push(PermissionRow {
                    request_id,
                    summary: permission_summary(params.as_ref()),
                    status: "pending".to_string(),
                    response_tx: Some(response_tx),
                });
                trim_resolved_permissions(&mut self.permissions, 8);
                self.status = "permission required".to_string();
            }
            TerminalEvent::TerminalInput { terminal_id, data } => {
                self.apply_terminal_bytes(&terminal_id, &data);
            }
            TerminalEvent::SessionUpdate(update) => self.apply_session_update(&update),
        }
    }

    fn apply_input(&mut self, input: &str) {
        match input.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" | "once" | "allow" | "approve" => {
                self.resolve_first_permission(PermissionChoice::AllowOnce)
            }
            "a" | "always" | "session" | "allow-session" => {
                self.resolve_first_permission(PermissionChoice::AllowSession)
            }
            "n" | "no" | "deny" | "reject" => self.resolve_first_permission(PermissionChoice::Deny),
            "" => {}
            other => {
                self.progress.push(format!("unhandled input: {other}"));
                trim_to(&mut self.progress, 10);
            }
        }
    }

    fn apply_terminal_bytes(&mut self, terminal_id: &str, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        self.progress.push(format!(
            "terminal {terminal_id} input: {} bytes",
            data.len()
        ));
        for line in text.split_terminator(['\n', '\r']) {
            if !line.trim().is_empty() {
                self.apply_input(line);
            }
        }
        trim_to(&mut self.progress, 10);
    }

    fn has_pending_permission(&self) -> bool {
        self.permissions
            .iter()
            .any(|permission| permission.response_tx.is_some())
    }

    fn resolve_first_permission(&mut self, choice: PermissionChoice) {
        let Some(row) = self
            .permissions
            .iter_mut()
            .find(|row| row.response_tx.is_some())
        else {
            self.progress
                .push("no pending permission request".to_string());
            trim_to(&mut self.progress, 10);
            return;
        };

        let Some(response_tx) = row.response_tx.take() else {
            return;
        };
        let response = JsonRpcResponse::success(row.request_id.clone(), permission_result(choice));
        let _ = response_tx.send(response);
        row.status = match choice {
            PermissionChoice::AllowOnce => "approved once",
            PermissionChoice::AllowSession => "approved session",
            PermissionChoice::Deny => "denied",
        }
        .to_string();
        self.status = format!("permission {}", row.status);
    }

    fn apply_session_update(&mut self, update: &Value) {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("operation_started") => {
                let operation = update
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("operation");
                self.status = format!("running: {operation}");
            }
            Some("operation_completed") => {
                let operation = update
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("operation");
                let success = update
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                self.status = if success {
                    format!("completed: {operation}")
                } else {
                    format!("stopped: {operation}")
                };
            }
            Some("agent_message_chunk") => {
                if let Some(text) = update
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                {
                    self.chat.push(text.trim_end().to_string());
                    trim_to(&mut self.chat, 24);
                }
            }
            Some("tool_call") => {
                let id = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("tool-call")
                    .to_string();
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("tool call")
                    .to_string();
                self.tools.push(ToolRow {
                    id,
                    title,
                    status: "started".to_string(),
                });
                trim_to(&mut self.tools, 12);
            }
            Some("tool_call_update") => {
                let id = update.get("toolCallId").and_then(Value::as_str);
                let status = update
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("updated");
                let title = update.get("title").and_then(Value::as_str);
                if let Some(row) = self
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|row| id.is_some_and(|id| row.id == id))
                {
                    row.status = status.to_string();
                    if let Some(title) = title {
                        row.title = title.to_string();
                    }
                }
            }
            Some("progress") => {
                if let Some(message) = update.get("message").and_then(Value::as_str) {
                    self.progress.push(message.to_string());
                    trim_to(&mut self.progress, 10);
                }
            }
            _ => {}
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("\x1b[2J\x1b[H");
        out.push_str("Brehon Native Agent\n");
        out.push_str("==================\n");
        out.push_str(&format!("provider: {}\n", self.provider));
        out.push_str(&format!("model: {}\n", self.model));
        out.push_str(&format!("acp: {} ({})\n", self.gateway, self.socket_path));
        out.push_str(&format!("status: {}\n\n", self.status));

        out.push_str("Chat\n----\n");
        if self.chat.is_empty() {
            out.push_str("waiting for prompt\n");
        } else {
            for line in &self.chat {
                out.push_str("assistant> ");
                out.push_str(line);
                out.push('\n');
            }
        }

        out.push_str("\nTools\n-----\n");
        if self.tools.is_empty() {
            out.push_str("no tool calls\n");
        } else {
            for tool in &self.tools {
                out.push_str(&format!("{}  {}\n", tool.status, tool.title));
            }
        }

        out.push_str("\nPermissions\n-----------\n");
        if self.permissions.is_empty() {
            out.push_str("no pending permission requests\n");
        } else {
            for permission in &self.permissions {
                out.push_str(&format!("{}  {}\n", permission.status, permission.summary));
            }
        }

        out.push_str("\nInput\n-----\n");
        if self
            .permissions
            .iter()
            .any(|permission| permission.response_tx.is_some())
        {
            out.push_str("type y to approve once, a to approve for session, or n to deny\n");
        } else {
            out.push_str("waiting\n");
        }

        if !self.progress.is_empty() {
            out.push_str("\nProgress\n--------\n");
            for message in &self.progress {
                out.push_str(message);
                out.push('\n');
            }
        }

        out
    }
}

pub(crate) async fn run_terminal_ui<R, W>(
    config: TerminalUiConfig,
    mut events: TerminalEventReceiver,
    reader: R,
    mut writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut state = TerminalUiState::new(config);
    let mut input = BufReader::new(reader).lines();
    let mut input_open = true;
    render(&state, &mut writer).await?;

    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    break;
                };
                state.apply_event(event);
                render(&state, &mut writer).await?;
            }
            line = input.next_line(), if input_open => {
                match line? {
                    Some(line) => {
                        state.apply_input(&line);
                        render(&state, &mut writer).await?;
                    }
                    None => input_open = false,
                }
            }
        }
    }

    Ok(())
}

pub(crate) async fn run_ratatui_terminal_ui(
    config: TerminalUiConfig,
    events: TerminalEventReceiver,
) -> std::io::Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return run_terminal_ui(config, events, tokio::io::stdin(), tokio::io::stdout()).await;
    }

    let mut events = events;
    let mut terminal = RatatuiTerminalGuard::enter()?;
    let mut state = TerminalUiState::new(config);
    let mut input_events = spawn_terminal_event_reader();
    terminal.draw(|frame| draw_ratatui(frame, &state))?;

    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    break;
                };
                state.apply_event(event);
                terminal.draw(|frame| draw_ratatui(frame, &state))?;
            }
            input = input_events.recv() => {
                let Some(input) = input else {
                    break;
                };
                if handle_crossterm_event(&mut state, input) {
                    return Ok(());
                }
                terminal.draw(|frame| draw_ratatui(frame, &state))?;
            }
            _ = tokio::time::sleep(Duration::from_millis(80)) => {
                terminal.draw(|frame| draw_ratatui(frame, &state))?;
            }
        }
    }

    Ok(())
}

fn spawn_terminal_event_reader() -> mpsc::UnboundedReceiver<CrosstermEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    thread::spawn(move || loop {
        match event::poll(Duration::from_millis(80)) {
            Ok(true) => match event::read() {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });
    rx
}

struct RatatuiTerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl RatatuiTerminalGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame<'_>),
    {
        self.terminal.draw(f).map(|_| ())
    }
}

impl Drop for RatatuiTerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
        let _ = self.terminal.backend_mut().flush();
    }
}

fn handle_crossterm_event(state: &mut TerminalUiState, event: CrosstermEvent) -> bool {
    match event {
        CrosstermEvent::Key(KeyEvent {
            code, modifiers, ..
        }) => match (code, modifiers) {
            (KeyCode::Char('q'), KeyModifiers::CONTROL)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => return true,
            (KeyCode::Char('y'), _) if state.has_pending_permission() => {
                state.resolve_first_permission(PermissionChoice::AllowOnce);
            }
            (KeyCode::Char('a'), _) if state.has_pending_permission() => {
                state.resolve_first_permission(PermissionChoice::AllowSession);
            }
            (KeyCode::Char('n'), _) if state.has_pending_permission() => {
                state.resolve_first_permission(PermissionChoice::Deny);
            }
            (KeyCode::Enter, _) => {
                let input = std::mem::take(&mut state.input);
                state.apply_input(&input);
            }
            (KeyCode::Backspace, _) => {
                state.input.pop();
            }
            (KeyCode::Char(ch), _) => {
                state.input.push(ch);
            }
            _ => {}
        },
        CrosstermEvent::Resize(_, _) => {}
        _ => {}
    }
    false
}

fn draw_ratatui(frame: &mut ratatui::Frame<'_>, state: &TerminalUiState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    draw_header(frame, root[0], state);
    draw_body(frame, root[1], state);
    draw_input(frame, root[2], state);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TerminalUiState) {
    let title = Line::from(vec![
        Span::styled(
            " BREHON native-agent ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("provider="),
        Span::styled(&state.provider, Style::default().fg(Color::White)),
        Span::raw(" model="),
        Span::styled(&state.model, Style::default().fg(Color::White)),
    ]);
    let status = Line::from(vec![
        Span::raw(" acp="),
        Span::styled(&state.gateway, Style::default().fg(Color::Green)),
        Span::raw(" status="),
        Span::styled(&state.status, Style::default().fg(Color::Yellow)),
        Span::raw(" socket="),
        Span::styled(&state.socket_path, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(
        Paragraph::new(vec![title, status]).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn draw_body(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TerminalUiState) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(columns[1]);

    draw_chat(frame, columns[0], state);
    draw_tools(frame, right[0], state);
    draw_permissions(frame, right[1], state);
    draw_progress(frame, right[2], state);
}

fn draw_chat(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TerminalUiState) {
    let lines = if state.chat.is_empty() {
        vec![Line::from(Span::styled(
            "waiting for prompt",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        state
            .chat
            .iter()
            .flat_map(|line| {
                line.lines()
                    .map(|part| {
                        Line::from(vec![
                            Span::styled("assistant ", Style::default().fg(Color::Cyan)),
                            Span::raw(part.to_string()),
                        ])
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Chat "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_tools(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TerminalUiState) {
    let items = state.tools.iter().rev().take(12).map(|tool| {
        ListItem::new(Line::from(vec![
            Span::styled(
                &tool.status,
                Style::default().fg(tool_status_color(&tool.status)),
            ),
            Span::raw(" "),
            Span::raw(&tool.title),
        ]))
    });
    frame.render_widget(
        List::new(items.collect::<Vec<_>>())
            .block(Block::default().borders(Borders::ALL).title(" Tools ")),
        area,
    );
}

fn draw_permissions(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TerminalUiState) {
    let items = if state.permissions.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "no pending permission requests",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        state
            .permissions
            .iter()
            .rev()
            .take(8)
            .map(|permission| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        &permission.status,
                        Style::default().fg(permission_status_color(&permission.status)),
                    ),
                    Span::raw(" "),
                    Span::raw(&permission.summary),
                ]))
            })
            .collect()
    };
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Permissions "),
        ),
        area,
    );
}

fn draw_progress(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TerminalUiState) {
    let lines = if state.progress.is_empty() {
        vec![Line::from(Span::styled(
            "no recent progress events",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        state
            .progress
            .iter()
            .rev()
            .take(10)
            .map(|line| Line::from(line.as_str()))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Runtime "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_input(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TerminalUiState) {
    let prompt = if state.has_pending_permission() {
        " y approve once | a approve session | n deny "
    } else {
        " input "
    };
    let value = if state.input.is_empty() {
        Span::styled(prompt.trim(), Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(state.input.as_str())
    };
    frame.render_widget(
        Paragraph::new(Line::from(value))
            .block(Block::default().borders(Borders::ALL).title(prompt)),
        area,
    );
}

fn tool_status_color(status: &str) -> Color {
    match status {
        "completed" => Color::Green,
        "failed" => Color::Red,
        _ => Color::Yellow,
    }
}

fn permission_status_color(status: &str) -> Color {
    match status {
        "pending" => Color::Yellow,
        "denied" => Color::Red,
        value if value.starts_with("approved") => Color::Green,
        _ => Color::Gray,
    }
}

async fn render<W>(state: &TerminalUiState, writer: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(state.render().as_bytes()).await?;
    writer.flush().await
}

fn permission_result(choice: PermissionChoice) -> Value {
    json!({
        "outcome": {
            "outcome": "selected",
            "optionId": match choice {
                PermissionChoice::AllowOnce => "allow-once",
                PermissionChoice::AllowSession => "allow-session",
                PermissionChoice::Deny => "deny",
            },
        }
    })
}

fn permission_summary(params: Option<&Value>) -> String {
    let Some(params) = params else {
        return "permission requested".to_string();
    };
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("write");
    let detail = params
        .get("details")
        .and_then(|details| {
            details
                .get("subject")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| details.get("arguments").map(compact_json))
        })
        .unwrap_or_default();
    if detail.is_empty() {
        format!("{kind}: {action}")
    } else {
        format!("{kind}: {action} {detail}")
    }
}

fn compact_json(value: &Value) -> String {
    const MAX_LEN: usize = 160;
    let mut raw = value.to_string();
    if raw.len() <= MAX_LEN {
        raw
    } else {
        let mut end = MAX_LEN;
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        raw.truncate(end);
        raw.push_str("...");
        raw
    }
}

fn trim_to<T>(items: &mut Vec<T>, max: usize) {
    if items.len() > max {
        items.drain(0..items.len() - max);
    }
}

fn trim_resolved_permissions(items: &mut Vec<PermissionRow>, max_resolved: usize) {
    let resolved = items
        .iter()
        .filter(|permission| permission.response_tx.is_none())
        .count();
    if resolved <= max_resolved {
        return;
    }
    let mut to_remove = resolved - max_resolved;
    items.retain(|permission| {
        if to_remove == 0 || permission.response_tx.is_some() {
            return true;
        }
        to_remove -= 1;
        false
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_input_resolves_pending_permission() {
        let (events_tx, events_rx) = event_channel();
        let (mut input_tx, input_rx) = tokio::io::duplex(64);
        let ui = tokio::spawn(run_terminal_ui(
            TerminalUiConfig {
                provider: "fake".to_string(),
                model: "fake-model".to_string(),
                socket_path: "/tmp/native.sock".to_string(),
            },
            events_rx,
            input_rx,
            tokio::io::sink(),
        ));

        let (response_tx, response_rx) = oneshot::channel();
        events_tx
            .send(TerminalEvent::PermissionRequest {
                request_id: "perm-1".to_string(),
                params: Some(json!({
                    "action": "write_file",
                    "kind": "write",
                    "details": {
                        "arguments": {
                            "path": "notes.txt",
                            "content": "ok"
                        }
                    }
                })),
                response_tx,
            })
            .expect("send permission event");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        input_tx.write_all(b"y\n").await.expect("write input");

        let response = response_rx.await.expect("permission response");
        assert_eq!(response.id, "perm-1");
        assert_eq!(
            response.result.unwrap()["outcome"]["optionId"].as_str(),
            Some("allow-once")
        );

        drop(events_tx);
        ui.await.expect("ui join").expect("ui ok");
    }

    #[tokio::test]
    async fn terminal_input_can_allow_permission_for_session() {
        let (events_tx, events_rx) = event_channel();
        let (mut input_tx, input_rx) = tokio::io::duplex(64);
        let ui = tokio::spawn(run_terminal_ui(
            TerminalUiConfig {
                provider: "fake".to_string(),
                model: "fake-model".to_string(),
                socket_path: "/tmp/native.sock".to_string(),
            },
            events_rx,
            input_rx,
            tokio::io::sink(),
        ));

        let (response_tx, response_rx) = oneshot::channel();
        events_tx
            .send(TerminalEvent::PermissionRequest {
                request_id: "perm-2".to_string(),
                params: Some(json!({
                    "action": "bash",
                    "kind": "execute",
                    "details": {
                        "subject": "cargo check"
                    }
                })),
                response_tx,
            })
            .expect("send permission event");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        input_tx.write_all(b"a\n").await.expect("write input");

        let response = response_rx.await.expect("permission response");
        assert_eq!(response.id, "perm-2");
        assert_eq!(
            response.result.unwrap()["outcome"]["optionId"].as_str(),
            Some("allow-session")
        );

        drop(events_tx);
        ui.await.expect("ui join").expect("ui ok");
    }
}
