//! Styled terminal output for CLI commands.
//!
//! Provides colored, formatted output using ANSI escape codes.
//! Respects NO_COLOR environment variable for accessibility.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::time::Instant;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::execute;
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};

/// Whether color output is enabled (respects NO_COLOR env).
fn color_enabled() -> bool {
    std::env::var("NO_COLOR").is_err()
}

// ANSI color codes
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const WHITE: &str = "\x1b[37m";
const BRIGHT_BLACK: &str = "\x1b[90m";

/// Apply ANSI style codes to text, respecting NO_COLOR.
fn styled(codes: &[&str], text: &str) -> String {
    if !color_enabled() {
        return text.to_string();
    }
    let prefix: String = codes.iter().copied().collect();
    format!("{}{}{}", prefix, text, RESET)
}

pub fn bold(text: &str) -> String {
    styled(&[BOLD], text)
}

pub fn dim(text: &str) -> String {
    styled(&[DIM], text)
}

pub fn green(text: &str) -> String {
    styled(&[GREEN], text)
}

pub fn red(text: &str) -> String {
    styled(&[RED], text)
}

pub fn cyan(text: &str) -> String {
    styled(&[CYAN], text)
}

pub fn yellow(text: &str) -> String {
    styled(&[YELLOW], text)
}

pub fn blue(text: &str) -> String {
    styled(&[BLUE], text)
}

pub fn magenta(text: &str) -> String {
    styled(&[MAGENTA], text)
}

pub fn bold_cyan(text: &str) -> String {
    styled(&[BOLD, CYAN], text)
}

pub fn bold_green(text: &str) -> String {
    styled(&[BOLD, GREEN], text)
}

pub fn bold_white(text: &str) -> String {
    styled(&[BOLD, WHITE], text)
}

pub fn bright_black(text: &str) -> String {
    styled(&[BRIGHT_BLACK], text)
}

// ── Splash palette (truecolor) ──────────────────────────────────────────────
// Cyan → violet gradient evokes the "brehon" as a gathering of distinct minds.
const BRAND_FROM: (u8, u8, u8) = (0x3C, 0xD3, 0xFF); // sky cyan
const BRAND_TO: (u8, u8, u8) = (0xB6, 0x5C, 0xFF); // lavender
const ACCENT_WARM: (u8, u8, u8) = (0xFF, 0xB0, 0x4C); // amber (supervisor/highlight)
const DIM_RULE: (u8, u8, u8) = (0x4A, 0x4F, 0x66); // muted frame

/// 10-frame braille spinner (standard).
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 6-row ASCII block logo — fixed 42 visible columns per row.
const LOGO: [&str; 6] = [
    " █████╗  ██████╗  ██████╗ ██████╗  █████╗ ",
    "██╔══██╗██╔════╝ ██╔═══██╗██╔══██╗██╔══██╗",
    "███████║██║  ███╗██║   ██║██████╔╝███████║",
    "██╔══██║██║   ██║██║   ██║██╔══██╗██╔══██║",
    "██║  ██║╚██████╔╝╚██████╔╝██║  ██║██║  ██║",
    "╚═╝  ╚═╝ ╚═════╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝",
];
const LOGO_WIDTH: usize = 42;
const TAGLINE: &str = "a gathering place for autonomous coding agents";

/// Linear interpolation between two bytes.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let v = a as f32 * (1.0 - t) + b as f32 * t;
    v.round().clamp(0.0, 255.0) as u8
}

/// Wrap `text` in a truecolor foreground SGR, respecting NO_COLOR.
fn truecolor(rgb: (u8, u8, u8), text: &str) -> String {
    if !color_enabled() {
        return text.to_string();
    }
    format!("\x1b[38;2;{};{};{}m{}{}", rgb.0, rgb.1, rgb.2, text, RESET)
}

/// Paint each character of `text` along a horizontal gradient.
/// Multi-byte Unicode (e.g. box-drawing blocks) is handled by `chars()`.
fn gradient_line(text: &str, from: (u8, u8, u8), to: (u8, u8, u8)) -> String {
    if !color_enabled() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let denom = (chars.len().saturating_sub(1)).max(1) as f32;
    let mut out = String::with_capacity(text.len() + chars.len() * 20);
    for (i, ch) in chars.iter().enumerate() {
        let t = (i as f32) / denom;
        let r = lerp_u8(from.0, to.0, t);
        let g = lerp_u8(from.1, to.1, t);
        let b = lerp_u8(from.2, to.2, t);
        out.push_str(&format!("\x1b[38;2;{};{};{}m", r, g, b));
        out.push(*ch);
    }
    out.push_str(RESET);
    out
}

/// Left pad needed to horizontally center visible `display_len` in `width`.
fn center_left_pad(width: usize, display_len: usize) -> usize {
    width.saturating_sub(display_len) / 2
}

fn truncate_plain(text: &str, budget: usize) -> String {
    let len = text.chars().count();
    if len <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    if budget == 1 {
        return "…".to_string();
    }
    let mut out: String = text.chars().take(budget - 1).collect();
    out.push('…');
    out
}

/// Parse the summary format `"{workers} workers, {reviewers} reviewers, supervisor {name}"`.
fn parse_summary(s: &str) -> (Option<usize>, Option<usize>, Option<String>) {
    let mut workers = None;
    let mut reviewers = None;
    let mut supervisor = None;
    for part in s.split(',') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("supervisor ") {
            supervisor = Some(rest.trim().to_string());
        } else if let Some(n) = p.strip_suffix(" workers") {
            workers = n.trim().parse().ok();
        } else if let Some(n) = p.strip_suffix(" reviewers") {
            reviewers = n.trim().parse().ok();
        }
    }
    (workers, reviewers, supervisor)
}

/// Structured roster of agent pools, used by the architecture diagram to
/// show *kinds* (claude, codex, kimi, …) rather than just totals.
#[derive(Clone, Default)]
struct Roster {
    workers: Vec<(String, u32)>,
    reviewers: Vec<(String, u32)>,
    supervisor_lane: String,
}

/// Take the portion of a lane name before the first `-`, which by convention
/// identifies the agent family (`claude-code` -> `claude`, `codex-acp` ->
/// `codex`). Falls back to the full lane if no dash is present.
fn short_kind(lane: &str) -> String {
    lane.split('-').next().unwrap_or(lane).to_string()
}

/// Collapse a `(lane, count)` list into `(kind, total_count)` preserving
/// first-seen order, so the diagram shows a stable layout across renders.
fn aggregate_by_kind(items: &[(String, u32)]) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for (lane, n) in items {
        let kind = short_kind(lane);
        if let Some(entry) = out.iter_mut().find(|e| e.0 == kind) {
            entry.1 += n;
        } else {
            out.push((kind, *n));
        }
    }
    out
}

/// Fit an aggregated kinds list into a tight visible-width `budget`, trying
/// progressively more compact formats:
///   1. `kind N  kind N  kind N`  (double-space separator, most readable)
///   2. `kind N kind N kind N`    (single-space separator)
///   3. `kind N kind N +K`        (drop trailing entries, add "+K more" badge)
///   4. `kind N…`                 (ellipsis truncation as the last resort)
fn kinds_within(items: &[(String, u32)], budget: usize) -> String {
    let agg = aggregate_by_kind(items);
    if agg.is_empty() {
        return String::new();
    }
    let pieces: Vec<String> = agg.iter().map(|(k, n)| format!("{} {}", k, n)).collect();

    let double = pieces.join("  ");
    if double.chars().count() <= budget {
        return double;
    }
    let single = pieces.join(" ");
    if single.chars().count() <= budget {
        return single;
    }
    // Greedy fit: keep as many leading entries as possible, annotate the rest
    // with a "+K" suffix so the operator knows more kinds are in play.
    for take in (1..pieces.len()).rev() {
        let head = pieces[..take].join(" ");
        let rest = pieces.len() - take;
        let candidate = format!("{} +{}", head, rest);
        if candidate.chars().count() <= budget {
            return candidate;
        }
    }
    // Last resort: truncate the single-entry rendering with an ellipsis.
    let mut s: String = single.chars().take(budget.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Event classification drives both the left glyph and the message colour.
#[derive(Clone, Copy)]
enum EventKind {
    Info,
    Step,
    Success,
    Warning,
    Error,
    Highlight,
}

fn classify_event(msg: &str) -> EventKind {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("failed") || lower.contains("error:") || lower.starts_with("error ") {
        EventKind::Error
    } else if lower.contains("warning") || lower.contains("stale ") {
        EventKind::Warning
    } else if lower.contains(" ready") || lower.ends_with(" ready") || lower.contains("ready at ") {
        EventKind::Success
    } else if lower.starts_with("planned launch") || lower.starts_with("project root") {
        EventKind::Highlight
    } else if lower.starts_with("creating ")
        || lower.starts_with("preparing ")
        || lower.starts_with("ensuring ")
        || lower.starts_with("reconciling ")
        || lower.starts_with("checking ")
    {
        EventKind::Step
    } else {
        EventKind::Info
    }
}

pub struct StartupSplash {
    active: bool,
    is_tty: bool,
    raw_mode_enabled: bool,
    stdout: std::io::Stdout,
    started_at: Instant,
    stage: String,
    summary: Option<String>,
    roster: Option<Roster>,
    events: VecDeque<(u64, String)>,
    frame_rows_written: usize,
    frame_row_budget: usize,
}

impl Default for StartupSplash {
    fn default() -> Self {
        Self::new()
    }
}

impl StartupSplash {
    pub fn new() -> Self {
        let mut stdout = std::io::stdout();
        // `BREHON_SPLASH_DEBUG_FORCE_RENDER` forces rendering to a non-TTY
        // stdout (useful for capturing a snapshot of the splash into a file
        // during development). We skip the alternate-screen/cursor toggles in
        // that mode since those only make sense for a real terminal.
        let force = std::env::var_os("BREHON_SPLASH_DEBUG_FORCE_RENDER").is_some();
        let is_tty = stdout.is_terminal();
        let active = is_tty || force;
        let mut raw_mode_enabled = false;
        if active && is_tty {
            raw_mode_enabled = terminal::enable_raw_mode().is_ok();
            let _ = execute!(
                stdout,
                EnterAlternateScreen,
                Hide,
                DisableLineWrap,
                Clear(ClearType::Purge),
                Clear(ClearType::All),
                MoveTo(0, 0)
            );
        }

        let mut splash = Self {
            active,
            is_tty,
            raw_mode_enabled,
            stdout,
            started_at: Instant::now(),
            stage: "Starting Brehon".to_string(),
            summary: None,
            roster: None,
            events: VecDeque::new(),
            frame_rows_written: 0,
            frame_row_budget: 0,
        };
        splash.render();
        splash
    }

    pub fn set_stage(&mut self, stage: impl Into<String>) {
        self.stage = stage.into();
        self.render();
    }

    pub fn set_summary(&mut self, summary: impl Into<String>) {
        self.summary = Some(summary.into());
        self.render();
    }

    /// Publish the structured roster (agent kinds × counts) that the
    /// architecture diagram renders. Each `(lane, count)` pair identifies one
    /// pool configured in `brehon.yaml`; duplicate kinds (e.g. two `claude-*`
    /// pools) are collapsed on screen.
    pub fn set_roster(
        &mut self,
        workers: Vec<(String, u32)>,
        reviewers: Vec<(String, u32)>,
        supervisor_lane: impl Into<String>,
    ) {
        self.roster = Some(Roster {
            workers,
            reviewers,
            supervisor_lane: supervisor_lane.into(),
        });
        self.render();
    }

    pub fn record(&mut self, event: impl Into<String>) {
        let secs = self.started_at.elapsed().as_secs();
        self.events.push_back((secs, event.into()));
        while self.events.len() > 200 {
            self.events.pop_front();
        }
        self.render();
    }

    pub fn finish(&mut self) {
        if !self.active {
            return;
        }

        if self.is_tty {
            let _ = execute!(
                self.stdout,
                Show,
                EnableLineWrap,
                MoveTo(0, 0),
                Clear(ClearType::Purge),
                Clear(ClearType::All),
                LeaveAlternateScreen
            );
            if self.raw_mode_enabled {
                let _ = terminal::disable_raw_mode();
                self.raw_mode_enabled = false;
            }
        }
        let _ = self.stdout.flush();
        self.active = false;
    }

    fn render(&mut self) {
        if !self.active {
            return;
        }

        let (cols, rows) = terminal::size().unwrap_or((100, 30));
        let width = cols as usize;
        let height = rows as usize;
        if width == 0 || height == 0 {
            return;
        }
        let drawable_height = if self.is_tty {
            // Keep one row as an inert guard row. Even with cursor-addressed
            // drawing, this protects against hosts that wrap wide glyphs at
            // the bottom edge.
            height.saturating_sub(1).max(1)
        } else {
            height
        };
        self.frame_rows_written = 0;
        self.frame_row_budget = drawable_height;

        let elapsed = self.started_at.elapsed();
        // Spinner frame derived from wall clock so animation advances naturally
        // as events stream in during startup.
        let spinner = SPINNER[(elapsed.as_millis() / 90) as usize % SPINNER.len()];

        if self.is_tty {
            let _ = execute!(
                self.stdout,
                MoveTo(0, 0),
                Clear(ClearType::Purge),
                Clear(ClearType::All),
                MoveTo(0, 0)
            );
        }

        if width < 24 || drawable_height < 8 {
            self.render_minimal(width, spinner);
            let _ = self.stdout.flush();
            return;
        }

        // Top-right elapsed badge.
        let elapsed_badge = format!(
            "⏱  {:02}:{:02}",
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60
        );
        let badge_pad = width.saturating_sub(elapsed_badge.chars().count() + 2);
        self.write_line(format!(
            "{}{}",
            " ".repeat(badge_pad),
            bright_black(&elapsed_badge),
        ));

        let logo_rows = if width >= 64 && drawable_height >= 15 {
            self.render_logo_card(width);
            11usize
        } else {
            self.render_compact_logo(width);
            2usize
        };

        // Architecture diagram — only draw if we have both vertical budget
        // AND at least one data source (summary string or structured roster)
        // so the agent counts/kinds are known.
        let show_arch = (self.summary.is_some() || self.roster.is_some())
            && drawable_height >= 30
            && width >= 70;
        if show_arch {
            self.write_blank_line();
            self.render_architecture(width);
        }

        self.write_blank_line();
        self.render_stage_bar(width, spinner);

        let rendered_so_far = if show_arch {
            1 + logo_rows + 1 + 13 + 1 + 2
        } else {
            1 + logo_rows + 1 + 2
        };
        let events_height = drawable_height.saturating_sub(rendered_so_far);
        self.render_activity(width, events_height);

        let _ = self.stdout.flush();
    }

    fn write_line(&mut self, line: impl AsRef<str>) {
        if self.frame_rows_written >= self.frame_row_budget {
            return;
        }

        if self.is_tty {
            let row = self.frame_rows_written.min(u16::MAX as usize) as u16;
            let _ = execute!(self.stdout, MoveTo(0, row));
            let _ = self.stdout.write_all(line.as_ref().as_bytes());
            self.frame_rows_written += 1;
            return;
        }

        let _ = self.stdout.write_all(line.as_ref().as_bytes());
        self.frame_rows_written += 1;

        if self.frame_rows_written < self.frame_row_budget {
            let ending = if self.raw_mode_enabled { "\r\n" } else { "\n" };
            let _ = self.stdout.write_all(ending.as_bytes());
        }
    }

    fn write_blank_line(&mut self) {
        self.write_line("");
    }

    fn render_minimal(&mut self, width: usize, spinner: &str) {
        let line = format!("{} {}", spinner, self.stage);
        self.write_line(line.chars().take(width).collect::<String>());
    }

    fn render_compact_logo(&mut self, width: usize) {
        let title = "BREHON";
        let title_pad = " ".repeat(center_left_pad(width, title.chars().count()));
        self.write_line(format!(
            "{}{}",
            title_pad,
            gradient_line(title, BRAND_FROM, BRAND_TO)
        ));

        let tagline = truncate_plain(TAGLINE, width);
        let tagline_pad = " ".repeat(center_left_pad(width, tagline.chars().count()));
        self.write_line(format!("{}{}", tagline_pad, dim(&tagline)));
    }

    /// Rounded frame around the gradient logo and Greek-label header.
    fn render_logo_card(&mut self, width: usize) {
        // Inner content width of 58 gives generous padding for the 42-wide logo.
        let inner = 58usize;
        let outer = inner + 2; // plus 2 border chars
        let left_pad = " ".repeat(center_left_pad(width, outer));

        // Top border with "BREHON" label embedded (5 visible chars).
        // Using Latin letters (not the Greek "ΑΓΟΡΑ") so the header renders
        // crisply in every monospace terminal font — many have weak Greek
        // capital glyphs that read as fuzzy/unrecognizable at small sizes.
        let label = " BREHON ";
        let label_visible = label.chars().count();
        let left_run = (inner.saturating_sub(label_visible)) / 2;
        let right_run = inner.saturating_sub(label_visible).saturating_sub(left_run);
        let top_line = format!(
            "{}{}{}{}{}",
            "╭",
            "─".repeat(left_run),
            label,
            "─".repeat(right_run),
            "╮",
        );
        self.write_line(format!("{}{}", left_pad, truecolor(DIM_RULE, &top_line)));

        let empty_row = format!("{}{}{}", "│", " ".repeat(inner), "│");
        self.write_line(format!("{}{}", left_pad, truecolor(DIM_RULE, &empty_row)));

        // Logo rows with a horizontal cyan→violet gradient.
        for line in &LOGO {
            let lp = (inner.saturating_sub(LOGO_WIDTH)) / 2;
            let rp = inner.saturating_sub(LOGO_WIDTH).saturating_sub(lp);
            let painted = gradient_line(line, BRAND_FROM, BRAND_TO);
            self.write_line(format!(
                "{}{}{}{}{}{}",
                left_pad,
                truecolor(DIM_RULE, "│"),
                " ".repeat(lp),
                painted,
                " ".repeat(rp),
                truecolor(DIM_RULE, "│"),
            ));
        }

        self.write_line(format!("{}{}", left_pad, truecolor(DIM_RULE, &empty_row)));

        // Tagline, centered within the card.
        let tag_len = TAGLINE.chars().count();
        let tp = (inner.saturating_sub(tag_len)) / 2;
        let trp = inner.saturating_sub(tag_len).saturating_sub(tp);
        self.write_line(format!(
            "{}{}{}{}{}{}",
            left_pad,
            truecolor(DIM_RULE, "│"),
            " ".repeat(tp),
            dim(TAGLINE),
            " ".repeat(trp),
            truecolor(DIM_RULE, "│"),
        ));

        let bottom = format!("{}{}{}", "╰", "─".repeat(inner), "╯");
        self.write_line(format!("{}{}", left_pad, truecolor(DIM_RULE, &bottom)));
    }

    /// Supervisor → Workers → Reviewers flow diagram, with a central MCP hub.
    ///
    /// Fixed geometry (inspired by the architecture of the Athenian brehon —
    /// three stoas opening onto a common bouleuterion):
    ///   - Card outer width: 20 (`╭` + 18 × `─` + `╮`)
    ///   - Inter-card gap:   3 (fits `──▶` exactly)
    ///   - Diagram width:    20 + 3 + 20 + 3 + 20 = 66
    ///   - Tee positions:    col 10 inside each card → cols 10, 33, 56
    fn render_architecture(&mut self, width: usize) {
        // Prefer the structured roster (kind × count) when it's available,
        // falling back to the parsed human-readable summary for back-compat.
        let (wc, rc, sup) = self
            .summary
            .as_deref()
            .map(parse_summary)
            .unwrap_or((None, None, None));
        let (workers_total, reviewers_total, sup_lane_full) = if let Some(r) = &self.roster {
            let wt: u32 = r.workers.iter().map(|(_, n)| n).sum();
            let rt: u32 = r.reviewers.iter().map(|(_, n)| n).sum();
            (
                Some(wt as usize),
                Some(rt as usize),
                r.supervisor_lane.clone(),
            )
        } else {
            (wc, rc, sup.unwrap_or_else(|| "claude".to_string()))
        };

        // Supervisor: row 1 shows the short kind ("claude"), row 2 shows the
        // full lane name dim-styled so operators can still identify the exact
        // instance (e.g. "claude-supervisor").
        let sup_kind: String = short_kind(&sup_lane_full).chars().take(13).collect();
        // inner_row2 truncates to 18 visible cols with an ellipsis as needed.
        let sup_sub: String = sup_lane_full.clone();

        let workers_txt = workers_total
            .map(|n| format!("{} agents", n))
            .unwrap_or_else(|| "pending".into());
        let reviewers_txt = reviewers_total
            .map(|n| format!("{} agents", n))
            .unwrap_or_else(|| "pending".into());

        // Compact kinds list for the sub-row. Falls back to a contextual
        // label when no roster was published (first-frame startup).
        // Row-2 budget == full card inner width (18 cols) so we maximise the
        // visible kind list.
        const ROW2_BUDGET: usize = 18;
        let workers_kinds = self
            .roster
            .as_ref()
            .map(|r| kinds_within(&r.workers, ROW2_BUDGET))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "coding in trees".into());
        let reviewers_kinds = self
            .roster
            .as_ref()
            .map(|r| kinds_within(&r.reviewers, ROW2_BUDGET))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "scoring PRs".into());

        const CARD_W: usize = 20;
        const GAP: usize = 3;
        const DIAGRAM_W: usize = CARD_W * 3 + GAP * 2; // 66
        let lpad = " ".repeat(center_left_pad(width, DIAGRAM_W));
        let rule = |s: &str| truecolor(DIM_RULE, s);
        let vbar = rule("│");

        // ── Title row: three labels centered in 20-col fields, 3-col gaps
        let title_sup = pad_centered(
            &truecolor(BRAND_FROM, "supervisor"),
            "supervisor".chars().count(),
            CARD_W,
        );
        let title_wk = pad_centered(
            &truecolor(BRAND_TO, "workers"),
            "workers".chars().count(),
            CARD_W,
        );
        let title_rv = pad_centered(
            &truecolor(ACCENT_WARM, "reviewers"),
            "reviewers".chars().count(),
            CARD_W,
        );
        self.write_line(format!(
            "{}{}{}{}{}{}",
            lpad,
            title_sup,
            " ".repeat(GAP),
            title_wk,
            " ".repeat(GAP),
            title_rv,
        ));

        // ── Top card borders
        let top = rule(&format!("╭{}╮", "─".repeat(CARD_W - 2)));
        self.write_line(format!(
            "{}{}{}{}{}{}",
            lpad,
            top,
            " ".repeat(GAP),
            top,
            " ".repeat(GAP),
            top,
        ));

        // ── Row 1: icon + primary content (inner 18 cols).
        // Inner layout: " X  FFFFFFFFFFFFFF " (1 + 1 + 2 + 13 + 1 = 18). We
        // truncate the field to 13 visible chars to keep alignment stable.
        let inner_row1 = |icon_rgb: (u8, u8, u8), icon: &str, text: &str| -> String {
            let trimmed: String = text.chars().take(13).collect();
            let field_plain = format!("{:<13}", trimmed);
            format!(
                " {}  {} ",
                truecolor(icon_rgb, icon),
                bold_white(&field_plain)
            )
        };
        let r1_sup = inner_row1(BRAND_FROM, "◉", &sup_kind);
        let r1_wk = inner_row1(BRAND_TO, "▰", &workers_txt);
        let r1_rv = inner_row1(ACCENT_WARM, "▰", &reviewers_txt);
        let arrow = rule("──▶");
        self.write_line(format!(
            "{}{}{}{}{}{}{}{}{}{}{}{}",
            lpad, vbar, r1_sup, vbar, arrow, vbar, r1_wk, vbar, arrow, vbar, r1_rv, vbar,
        ));

        // ── Row 2: dim kind-breakdown (or contextual label). Uses the full
        // 18-col inner width; overflow is truncated with an ellipsis so card
        // alignment stays pixel-perfect regardless of roster size.
        let inner_row2 = |text: &str| -> String {
            let width = 18usize;
            let chars: Vec<char> = text.chars().collect();
            let body = if chars.len() > width {
                let mut s: String = chars.iter().take(width - 1).collect();
                s.push('…');
                s
            } else {
                format!("{:<width$}", text, width = width)
            };
            dim(&body)
        };
        let r2_sup = inner_row2(&sup_sub);
        let r2_wk = inner_row2(&workers_kinds);
        let r2_rv = inner_row2(&reviewers_kinds);
        let gap3 = " ".repeat(GAP);
        self.write_line(format!(
            "{}{}{}{}{}{}{}{}{}{}{}{}",
            lpad, vbar, r2_sup, vbar, gap3, vbar, r2_wk, vbar, gap3, vbar, r2_rv, vbar,
        ));

        // ── Bottom card borders with `┬` at inner column 9 (i.e. outer col 10
        // within each card). In a 20-wide card this is: ╰ + 9× ─ + ┬ + 8× ─ + ╯
        let bot = rule(&format!("╰{}┬{}╯", "─".repeat(9), "─".repeat(8)));
        self.write_line(format!(
            "{}{}{}{}{}{}",
            lpad,
            bot,
            " ".repeat(GAP),
            bot,
            " ".repeat(GAP),
            bot,
        ));

        // ── Tees from cards meet above the hub.
        //
        // Tee columns within the diagram (0-indexed, measured from lpad):
        //   card1 starts at 0  → tee at col 10
        //   card2 starts at 23 → tee at col 33
        //   card3 starts at 46 → tee at col 56
        // Vertical-bar row:
        let bar_row = {
            let mut s = String::new();
            s.push_str(&" ".repeat(10));
            s.push_str(&rule("│"));
            s.push_str(&" ".repeat(33 - 11));
            s.push_str(&rule("│"));
            s.push_str(&" ".repeat(56 - 34));
            s.push_str(&rule("│"));
            s
        };
        self.write_line(format!("{}{}", lpad, bar_row));

        // Convergence row: ╰── … ──┴── … ──╯
        // Spans col 10 (╰) through col 56 (╯), with ┴ at col 33.
        let conv_row = {
            let mut s = String::new();
            s.push_str(&" ".repeat(10));
            s.push_str(&rule("╰"));
            s.push_str(&rule(&"─".repeat(33 - 11)));
            s.push_str(&rule("┴"));
            s.push_str(&rule(&"─".repeat(56 - 34)));
            s.push_str(&rule("╯"));
            s
        };
        self.write_line(format!("{}{}", lpad, conv_row));

        // Drop to MCP hub.
        let drop_row = {
            let mut s = String::new();
            s.push_str(&" ".repeat(33));
            s.push_str(&rule("│"));
            s
        };
        self.write_line(format!("{}{}", lpad, drop_row));
        let arrow_row = {
            let mut s = String::new();
            s.push_str(&" ".repeat(33));
            s.push_str(&truecolor(BRAND_TO, "▼"));
            s
        };
        self.write_line(format!("{}{}", lpad, arrow_row));

        // ── MCP hub card, centered at diagram col 33.
        const HUB_W: usize = 28;
        let hub_center_col = 33usize;
        let hub_lpad_in_diagram = hub_center_col.saturating_sub(HUB_W / 2);
        let hub_lpad = format!("{}{}", lpad, " ".repeat(hub_lpad_in_diagram));
        let hub_top = rule(&format!("╭{}╮", "─".repeat(HUB_W - 2)));
        let hub_bot = rule(&format!("╰{}╯", "─".repeat(HUB_W - 2)));

        // Hub row 1: "⬡  MCP — shared hub" = 19 visible cols, fits in inner=26.
        let hub_text1_plain = "MCP  —  shared hub";
        let t1_len = 1 + 2 + hub_text1_plain.chars().count(); // ⬡ + 2sp + text
        let t1_l = (HUB_W - 2).saturating_sub(t1_len) / 2;
        let t1_r = (HUB_W - 2).saturating_sub(t1_len).saturating_sub(t1_l);
        let hub_row1 = format!(
            "{}{}{}{}{}{}{}",
            rule("│"),
            " ".repeat(t1_l),
            truecolor(BRAND_TO, "⬡"),
            "  ",
            bold_white(hub_text1_plain),
            " ".repeat(t1_r),
            rule("│"),
        );

        let hub_text2_plain = "memories · rules · tasks";
        let t2_len = hub_text2_plain.chars().count();
        let t2_l = (HUB_W - 2).saturating_sub(t2_len) / 2;
        let t2_r = (HUB_W - 2).saturating_sub(t2_len).saturating_sub(t2_l);
        let hub_row2 = format!(
            "{}{}{}{}{}",
            rule("│"),
            " ".repeat(t2_l),
            dim(hub_text2_plain),
            " ".repeat(t2_r),
            rule("│"),
        );

        self.write_line(format!("{}{}", hub_lpad, hub_top));
        self.write_line(format!("{}{}", hub_lpad, hub_row1));
        self.write_line(format!("{}{}", hub_lpad, hub_row2));
        self.write_line(format!("{}{}", hub_lpad, hub_bot));
    }

    /// Stage line with animated spinner + right-aligned summary hint.
    fn render_stage_bar(&mut self, width: usize, spinner: &str) {
        let stage_line = format!(
            "  {}  {}",
            truecolor(BRAND_FROM, spinner),
            bold_white(&self.stage),
        );
        let left_visible = 2 + 1 + 2 + self.stage.chars().count();

        // Right-side summary, only drawn when there's at least a small gap
        // between it and the stage text. Otherwise the stage line stands alone.
        let summary_right = self
            .summary
            .as_deref()
            .map(|s| format!("plan: {}", s))
            .unwrap_or_else(|| "discovering agents…".to_string());
        let right_visible = summary_right.chars().count();
        const MIN_GAP: usize = 4;
        if left_visible + right_visible + MIN_GAP <= width {
            let fill = width
                .saturating_sub(left_visible)
                .saturating_sub(right_visible)
                .saturating_sub(2);
            self.write_line(format!(
                "{}{}{}",
                stage_line,
                " ".repeat(fill),
                dim(&summary_right),
            ));
        } else {
            self.write_line(stage_line);
        }

        // Thin divider with an inline "activity" label.
        let divider_inner_width = width.saturating_sub(4);
        let label = " activity ";
        let dash_left = 3;
        let dash_right = divider_inner_width.saturating_sub(label.chars().count() + dash_left);
        let divider = format!(
            "  {}{}{}",
            "─".repeat(dash_left),
            label,
            "─".repeat(dash_right),
        );
        self.write_line(truecolor(DIM_RULE, &divider));
    }

    /// Scrolling coloured activity feed.
    fn render_activity(&mut self, width: usize, events_height: usize) {
        let start = self.events.len().saturating_sub(events_height);
        let events: Vec<(u64, String)> = self.events.iter().skip(start).cloned().collect();
        for (secs, msg) in events {
            let kind = classify_event(&msg);
            let (glyph_raw, glyph_styled, styled_msg) = match kind {
                EventKind::Success => ("✓", green("✓"), green(&msg)),
                EventKind::Warning => ("⚠", yellow("⚠"), yellow(&msg)),
                EventKind::Error => ("✗", red("✗"), red(&msg)),
                EventKind::Highlight => ("◆", magenta("◆"), bold_white(&msg)),
                EventKind::Step => ("⟳", cyan("⟳"), cyan(&msg)),
                EventKind::Info => ("▸", dim("▸"), msg.clone()),
            };
            let _ = glyph_raw;
            let ts = bright_black(&format!("{:02}:{:02}", secs / 60, secs % 60));

            // Truncate the visible message to fit available width.
            let prefix_visible = 3 + 5 + 2 + 1 + 2; // "   " + "MM:SS" + "  " + glyph + "  "
            let max_msg = width.saturating_sub(prefix_visible + 1);
            let truncated_msg = if msg.chars().count() > max_msg {
                let mut t: String = msg.chars().take(max_msg.saturating_sub(1)).collect();
                t.push('…');
                // Recolor the truncated text to match the kind.
                match kind {
                    EventKind::Success => green(&t),
                    EventKind::Warning => yellow(&t),
                    EventKind::Error => red(&t),
                    EventKind::Highlight => bold_white(&t),
                    EventKind::Step => cyan(&t),
                    EventKind::Info => t,
                }
            } else {
                styled_msg
            };

            self.write_line(format!("   {}  {}  {}", ts, glyph_styled, truncated_msg));
        }
    }
}

/// Centre a pre-styled string inside `total_visible` columns using its
/// `visible_len` (character count with styling stripped).
fn pad_centered(styled: &str, visible_len: usize, total_visible: usize) -> String {
    let l = total_visible.saturating_sub(visible_len) / 2;
    let r = total_visible.saturating_sub(visible_len).saturating_sub(l);
    format!("{}{}{}", " ".repeat(l), styled, " ".repeat(r))
}

impl Drop for StartupSplash {
    fn drop(&mut self) {
        self.finish();
    }
}

/// User-visible feedback for the shutdown sequence between TUI exit and
/// process exit. The 30-second drain plus worktree cleanup can otherwise
/// look like a hang from the user's perspective; printing each phase to
/// stderr with elapsed time confirms progress is being made.
///
/// Output goes to stderr so any caller piping stdout (e.g. `brehon run | tee`)
/// keeps clean output channels. Append-style prints preserve scrollback so
/// the user can read what happened after the process exits.
pub struct ShutdownProgress {
    started_at: Instant,
    active: bool,
}

impl ShutdownProgress {
    /// Begin tracking shutdown. Prints a leading blank line + bold header
    /// so the shutdown phase is visually distinct from the TUI's last
    /// rendered frame.
    pub fn start() -> Self {
        eprintln!();
        eprintln!("{}", bold("Shutting down Brehon..."));
        Self {
            started_at: Instant::now(),
            active: true,
        }
    }

    fn elapsed_label(&self) -> String {
        format!("{:5.1}s", self.started_at.elapsed().as_secs_f64())
    }

    /// Print a normal progress step (cyan ›).
    pub fn step(&self, message: impl AsRef<str>) {
        if !self.active {
            return;
        }
        eprintln!(
            "  {} {}  {}",
            dim(&self.elapsed_label()),
            cyan("›"),
            message.as_ref()
        );
    }

    /// Print a warning step (yellow !) — used for non-fatal issues like
    /// drain timeouts or surviving processes.
    pub fn warn(&self, message: impl AsRef<str>) {
        if !self.active {
            return;
        }
        eprintln!(
            "  {} {}  {}",
            dim(&self.elapsed_label()),
            yellow("!"),
            message.as_ref()
        );
    }

    /// Print the final "complete" line (green ✓) and mark this progress
    /// instance as inactive so further calls become no-ops.
    pub fn finish(mut self) {
        if !self.active {
            return;
        }
        eprintln!(
            "  {} {}  Shutdown complete ({:.1}s)",
            dim(&self.elapsed_label()),
            green("✓"),
            self.started_at.elapsed().as_secs_f64()
        );
        self.active = false;
    }
}

/// Print the Brehon ASCII art banner with border.
pub fn print_banner() {
    // Each line is exactly 46 visible characters wide (accounting for
    // multi-byte Unicode block characters which are single-width in terminals).
    let logo_lines = [
        " █████╗  ██████╗  ██████╗ ██████╗  █████╗ ",
        "██╔══██╗██╔════╝ ██╔═══██╗██╔══██╗██╔══██╗",
        "███████║██║  ███╗██║   ██║██████╔╝███████║ ",
        "██╔══██║██║   ██║██║   ██║██╔══██╗██╔══██║ ",
        "██║  ██║╚██████╔╝╚██████╔╝██║  ██║██║  ██║",
        "╚═╝  ╚═╝ ╚═════╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝",
    ];

    // Use a fixed inner width that fits the logo + padding
    let inner_width = 52;
    let border = |left: &str, fill: &str, right: &str| {
        format!(
            "  {}{}{}",
            dim(left),
            dim(&fill.repeat(inner_width)),
            dim(right)
        )
    };
    let empty = format!("  {}{}{}", dim("│"), " ".repeat(inner_width), dim("│"));

    println!();
    println!("{}", border("╭", "─", "╮"));
    println!("{}", empty);

    for line in &logo_lines {
        // Pad to inner_width using spaces (pad plain text, then style)
        let display_len = line.chars().count();
        let left_pad = (inner_width.saturating_sub(display_len)) / 2;
        let right_pad = inner_width
            .saturating_sub(display_len)
            .saturating_sub(left_pad);
        let padded = format!("{}{}{}", " ".repeat(left_pad), line, " ".repeat(right_pad));
        println!("  {}{}{}", dim("│"), bold_cyan(&padded), dim("│"));
    }

    println!("{}", empty);

    let tagline = "Multi-agent orchestration for AI coding agents";
    let tag_len = tagline.len();
    let left_pad = (inner_width.saturating_sub(tag_len)) / 2;
    let right_pad = inner_width.saturating_sub(tag_len).saturating_sub(left_pad);
    println!(
        "  {}{}{}{}{}",
        dim("│"),
        " ".repeat(left_pad),
        dim(tagline),
        " ".repeat(right_pad),
        dim("│"),
    );

    println!("{}", empty);
    println!("{}", border("╰", "─", "╯"));
    println!();
}

/// Print a success line: "  ✓ message"
pub fn print_success(msg: &str) {
    println!("  {} {}", green("✓"), msg);
}

/// Print a failure line: "  ✗ message"
pub fn print_failure(msg: &str) {
    println!("  {} {}", red("✗"), msg);
}

/// Print a warning line: "  ! message"
pub fn print_warning(msg: &str) {
    println!("  {} {}", yellow("!"), msg);
}

/// Print a section header.
pub fn print_section(title: &str) {
    println!("  {}", bold_white(title));
    println!();
}

/// Print a horizontal rule.
pub fn print_rule() {
    println!("  {}", dim(&"─".repeat(52)));
}

/// Agent detection result for display.
pub struct AgentCheck {
    pub name: String,
    pub command: String,
    pub description: String,
    pub found: bool,
    pub path: Option<String>,
}

/// Print agent detection results in a formatted list.
pub fn print_agent_checks(agents: &[&AgentCheck]) {
    for agent in agents {
        // Pad the name before styling so alignment is correct
        let padded_name = format!("{:<12}", agent.name);
        if agent.found {
            println!(
                "  {} {} {}  {}",
                green("✓"),
                bold(&padded_name),
                dim(&agent.description),
                bright_black(agent.path.as_deref().unwrap_or("")),
            );
        } else {
            println!(
                "  {} {} {}  {}",
                red("✗"),
                dim(&padded_name),
                dim(&agent.description),
                bright_black("not found"),
            );
        }
    }
}

/// Print a simple two-column table.
///
/// Pads plain text first, then applies ANSI styling, so column widths
/// are calculated on visible characters only.
pub fn print_table(headers: (&str, &str), rows: &[(&str, &str)]) {
    let col1_width = rows
        .iter()
        .map(|(a, _)| a.len())
        .max()
        .unwrap_or(0)
        .max(headers.0.len())
        + 2;
    let col2_width = rows
        .iter()
        .map(|(_, b)| b.len())
        .max()
        .unwrap_or(0)
        .max(headers.1.len())
        + 2;

    let h_rule = format!(
        "  {}{}{}{}{}",
        dim("├"),
        dim(&"─".repeat(col1_width)),
        dim("┼"),
        dim(&"─".repeat(col2_width)),
        dim("┤"),
    );

    println!(
        "  {}{}{}{}{}",
        dim("┌"),
        dim(&"─".repeat(col1_width)),
        dim("┬"),
        dim(&"─".repeat(col2_width)),
        dim("┐"),
    );

    // Pad headers before styling
    let h1 = format!("{:<w$}", headers.0, w = col1_width);
    let h2 = format!("{:<w$}", headers.1, w = col2_width);
    println!(
        "  {}{}{}{}{}",
        dim("│"),
        bold(&h1),
        dim("│"),
        bold(&h2),
        dim("│"),
    );
    println!("{}", h_rule);

    for (a, b) in rows {
        // Pad plain text before applying color
        let padded_a = format!("{:<w$}", a, w = col1_width);
        let padded_b = format!("{:<w$}", b, w = col2_width);
        println!(
            "  {}{}{}{}{}",
            dim("│"),
            cyan(&padded_a),
            dim("│"),
            padded_b,
            dim("│"),
        );
    }

    println!(
        "  {}{}{}{}{}",
        dim("└"),
        dim(&"─".repeat(col1_width)),
        dim("┴"),
        dim(&"─".repeat(col2_width)),
        dim("┘"),
    );
}

/// Print numbered steps.
pub fn print_steps(steps: &[&str]) {
    for (i, step) in steps.iter().enumerate() {
        println!("    {}  {}", dim(&format!("{}.", i + 1)), step,);
    }
}
