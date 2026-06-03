//! TUI theme tokens and helpers.
//!
//! Centralises every colour, glyph, and border decision so that no
//! `Color::Rgb(...)` literal lives outside this file.
//! Palette extracted from the splash (`crates/brehon-cli/src/ui.rs`) so
//! both surfaces render from one source of truth.

use std::time::Instant;

use ratatui::style::{Color, Style};
use ratatui::text::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Success,
    Running,
    Error,
    Warning,
    Info,
    Blocked,
    Idle,
}

// ── brand ──────────────────────────────────────────────────────────────────

/// Splash palette — cyan → violet gradient evokes the "brehon" as a
/// gathering of distinct minds.
pub mod brand {
    use super::Color;

    /// Sky cyan — start of the brand gradient.
    pub const PRIMARY_RGB: (u8, u8, u8) = (0x3C, 0xD3, 0xFF);
    /// Lavender — end of the brand gradient.
    pub const SECONDARY_RGB: (u8, u8, u8) = (0xB6, 0x5C, 0xFF);
    /// Amber — supervisor / highlight accent.
    pub const ACCENT_RGB: (u8, u8, u8) = (0xFF, 0xB0, 0x4C);
    /// Muted frame / rule colour.
    pub const DIM_RULE_RGB: (u8, u8, u8) = (0x4A, 0x4F, 0x66);

    /// Sky cyan — start of the brand gradient.
    pub const PRIMARY: Color = Color::Rgb(PRIMARY_RGB.0, PRIMARY_RGB.1, PRIMARY_RGB.2);
    /// Lavender — end of the brand gradient.
    pub const SECONDARY: Color = Color::Rgb(SECONDARY_RGB.0, SECONDARY_RGB.1, SECONDARY_RGB.2);
    /// Amber — supervisor / highlight accent.
    pub const ACCENT: Color = Color::Rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2);
    /// Muted frame / rule colour.
    pub const DIM_RULE: Color = Color::Rgb(DIM_RULE_RGB.0, DIM_RULE_RGB.1, DIM_RULE_RGB.2);

    /// Linear interpolation between two bytes.
    fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
        let v = a as f32 * (1.0 - t) + b as f32 * t;
        v.round().clamp(0.0, 255.0) as u8
    }

    /// Paint each character of `text` along a horizontal gradient from
    /// `from` to `to`, returning a `ratatui::text::Line` of individually
    /// coloured spans.
    pub fn gradient(from: (u8, u8, u8), to: (u8, u8, u8), text: &str) -> ratatui::text::Line<'_> {
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return ratatui::text::Line::default();
        }
        let denom = (chars.len().saturating_sub(1)).max(1) as f32;
        let spans: Vec<ratatui::text::Span> = chars
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                let t = (i as f32) / denom;
                let r = lerp_u8(from.0, to.0, t);
                let g = lerp_u8(from.1, to.1, t);
                let b = lerp_u8(from.2, to.2, t);
                ratatui::text::Span::styled(
                    ch.to_string(),
                    ratatui::style::Style::default().fg(Color::Rgb(r, g, b)),
                )
            })
            .collect();
        ratatui::text::Line::from(spans)
    }
}

// ── chrome ─────────────────────────────────────────────────────────────────

/// Window-dressing colours — backgrounds, borders, text tiers.
pub mod chrome {
    use super::brand;
    use super::Color;

    pub const BG: Color = Color::Rgb(30, 30, 35);
    pub const BG_ELEVATED: Color = Color::Rgb(45, 45, 55);
    pub const BORDER: Color = Color::Rgb(60, 60, 70);
    pub const BORDER_FOCUSED: Color = brand::PRIMARY;
    pub const RULE: Color = Color::Rgb(70, 70, 85);
    pub const TEXT: Color = Color::White;
    pub const TEXT_DIM: Color = Color::Rgb(160, 160, 170);
    pub const TEXT_MUTED: Color = Color::Rgb(120, 120, 130);
    pub const TEXT_LABEL: Color = Color::Rgb(100, 110, 130);
    pub const TEXT_BODY: Color = Color::Rgb(200, 200, 210);
    pub const TEXT_SOFT: Color = Color::Rgb(210, 220, 235);
    pub const TEXT_PATH: Color = Color::Rgb(185, 195, 210);
    pub const RULE_SUBTLE: Color = Color::Rgb(50, 58, 74);
    pub const RULE_STRONG: Color = Color::Rgb(55, 62, 74);
    pub const FOOTER_RULE: Color = Color::Rgb(50, 65, 90);
    pub const FOOTER_LABEL: Color = Color::Rgb(80, 88, 100);
    pub const PANEL_MATTE_BORDER: Color = Color::Rgb(28, 34, 46);
    pub const PANEL_MATTE_BG: Color = Color::Rgb(10, 14, 22);
    pub const PANEL_BORDER: Color = Color::Rgb(80, 110, 160);
    pub const PANEL_BG: Color = Color::Rgb(16, 20, 30);
    pub const PANEL_BG_ELEVATED: Color = Color::Rgb(20, 26, 38);
    pub const PANEL_BORDER_ELEVATED: Color = Color::Rgb(140, 190, 255);
}

// ── status ─────────────────────────────────────────────────────────────────

/// Semantic status colours.
pub mod status {
    use super::chrome;
    use super::Color;

    pub const PENDING: Color = Color::Rgb(200, 200, 100);
    pub const ASSIGNED: Color = Color::Rgb(180, 180, 120);
    pub const IN_PROGRESS: Color = Color::Rgb(100, 180, 255);
    pub const REVIEW_READY: Color = Color::Rgb(160, 150, 255);
    pub const IN_REVIEW: Color = Color::Rgb(180, 130, 255);
    pub const APPROVED: Color = Color::Rgb(80, 220, 160);
    pub const INTEGRATED: Color = Color::Rgb(60, 200, 180);
    pub const SUCCESS: Color = Color::Rgb(80, 200, 120);
    pub const RUNNING: Color = Color::Rgb(255, 200, 80);
    pub const ERROR: Color = Color::Rgb(255, 100, 100);
    pub const WARNING: Color = Color::Rgb(255, 180, 80);
    pub const INFO: Color = Color::Rgb(110, 185, 255);
    pub const BLOCKED: Color = Color::Rgb(255, 205, 125);
    pub const CONFLICT: Color = Color::Rgb(255, 120, 120);
    pub const REJECTED: Color = Color::Rgb(255, 80, 80);
    pub const IDLE: Color = chrome::TEXT_MUTED;
}

pub fn status_style(kind: StatusKind) -> Style {
    let color = match kind {
        StatusKind::Success => status::SUCCESS,
        StatusKind::Running => status::RUNNING,
        StatusKind::Error => status::ERROR,
        StatusKind::Warning => status::WARNING,
        StatusKind::Info => status::INFO,
        StatusKind::Blocked => status::BLOCKED,
        StatusKind::Idle => status::IDLE,
    };

    Style::default().fg(color)
}

// ── agent ──────────────────────────────────────────────────────────────────

/// Adapter → colour mapping.  Keeps every agent visually distinct at a
/// glance.
pub mod agent {
    use super::chrome;
    use super::Color;

    pub const CLAUDE: Color = Color::Rgb(204, 120, 50);
    pub const CODEX: Color = Color::Rgb(0, 170, 80);
    pub const GEMINI: Color = Color::Rgb(66, 133, 244);
    pub const OPENCODE: Color = Color::Rgb(200, 200, 200);
    pub const JUNIE: Color = Color::Rgb(255, 100, 100);
    pub const COPILOT: Color = Color::Rgb(100, 180, 255);

    pub fn color(adapter: &str) -> Color {
        match adapter {
            "claude" => CLAUDE,
            "codex" => CODEX,
            "gemini" => GEMINI,
            "opencode" => OPENCODE,
            "junie" => JUNIE,
            "copilot" => COPILOT,
            _ => chrome::TEXT,
        }
    }
}

// ── role ───────────────────────────────────────────────────────────────────

/// Pane role → colour and glyph mapping.
pub mod role {
    use super::Color;
    use brehon_mux::PaneKind;

    pub const WORKER: Color = Color::Rgb(100, 149, 237);
    pub const SUPERVISOR: Color = Color::Rgb(255, 200, 80);
    pub const REVIEWER: Color = Color::Rgb(80, 200, 120);
    pub const ADVISOR: Color = Color::Rgb(230, 160, 70);
    pub const RESEARCH: Color = Color::Rgb(105, 210, 190);
    pub const DIRECTOR: Color = Color::Rgb(180, 130, 255);
    pub const SHELL: Color = Color::Rgb(180, 180, 180);

    pub fn color(kind: &PaneKind) -> Color {
        match kind {
            PaneKind::Worker => WORKER,
            PaneKind::Supervisor => SUPERVISOR,
            PaneKind::Reviewer => REVIEWER,
            PaneKind::Advisor => ADVISOR,
            PaneKind::Research => RESEARCH,
            PaneKind::Director => DIRECTOR,
            PaneKind::Shell => SHELL,
        }
    }

    pub fn glyph(kind: &PaneKind) -> &'static str {
        match kind {
            PaneKind::Supervisor => "◉",
            PaneKind::Worker => "▰",
            PaneKind::Reviewer => "⬡",
            PaneKind::Advisor => "◇",
            PaneKind::Research => "◎",
            PaneKind::Director => "◆",
            PaneKind::Shell => "▫",
        }
    }
}

// ── detail ─────────────────────────────────────────────────────────────────

/// Additional semantic colours used by dashboard/detail chrome.
pub mod detail {
    use super::Color;

    pub const TASK_HINT: Color = Color::Rgb(255, 170, 120);
    pub const BLOCKED_BY: Color = Color::Rgb(255, 140, 110);
    pub const REVIEW_FEEDBACK: Color = Color::Rgb(255, 150, 110);
    pub const CONSTRAINTS: Color = Color::Rgb(255, 140, 140);
    pub const FINDING_BLOCKING: Color = Color::Rgb(255, 110, 110);
    pub const FINDING_SUGGESTION: Color = Color::Rgb(255, 200, 110);
    pub const FINDING_NITPICK: Color = Color::Rgb(180, 180, 220);
    pub const FINDING_DISSENT: Color = Color::Rgb(255, 160, 200);
    pub const ACTIVE_BADGE: Color = Color::Rgb(140, 220, 170);
    pub const MUTED_ACCENT: Color = Color::Rgb(120, 180, 255);
    pub const STATUS_BADGE_TEXT: Color = Color::Rgb(20, 20, 28);
}

// ── glyph ──────────────────────────────────────────────────────────────────

/// Reusable glyphs and the braille spinner sequence.
pub mod glyph {
    pub const CHECK: &str = "✓";
    pub const CROSS: &str = "✗";
    pub const BULLET: &str = "•";
    pub const DIAMOND: &str = "◆";
    pub const RUNNING: &str = "⟳";
    pub const ARROW_RIGHT: &str = "──▶";
    pub const ARROW_DOWN: &str = "▼";
    pub const ELLIPSIS: &str = "…";
    pub const WARNING: &str = "⚠";

    /// 10-frame braille spinner (standard).
    pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    /// Return the spinner frame for a given wall-clock tick.
    pub fn spinner(tick: usize) -> &'static str {
        SPINNER_FRAMES[tick % SPINNER_FRAMES.len()]
    }
}

// ── rule ───────────────────────────────────────────────────────────────────

/// Shared border and layout decisions.
pub mod rule {
    use ratatui::widgets::BorderType;

    /// Rounded corners are the only border style in the TUI.
    pub const BORDER_TYPE: BorderType = BorderType::Rounded;
}

/// Return the splash-format elapsed badge (`⏱  MM:SS`) for a started instant.
pub fn elapsed_badge(started: Instant) -> Span<'static> {
    let elapsed = started.elapsed();
    Span::styled(
        format!(
            "⏱  {:02}:{:02}",
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60
        ),
        Style::default().fg(chrome::TEXT_DIM),
    )
}
