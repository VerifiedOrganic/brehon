//! Rendering for the multiplexer using ratatui
//!
//! Handles rendering multiple panes to the terminal.

use crate::mux::Mux;
use crate::pane::Pane;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph};

/// Layout direction for panes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutDirection {
    #[default]
    Horizontal,
    Vertical,
}

/// Renderer for the multiplexer
#[derive(Default)]
pub struct Renderer {
    /// Layout direction
    direction: LayoutDirection,
    /// Whether to show borders
    border: bool,
}

impl Renderer {
    /// Create a new renderer
    pub fn new() -> Self {
        Self {
            direction: LayoutDirection::Horizontal,
            border: true,
        }
    }

    /// Set layout direction
    pub fn set_direction(&mut self, direction: LayoutDirection) {
        self.direction = direction;
    }

    /// Enable/disable borders
    pub fn set_border(&mut self, border: bool) {
        self.border = border;
    }

    /// Calculate layout constraints for panes
    fn calculate_constraints(&self, pane_count: usize) -> Vec<Constraint> {
        if pane_count == 0 {
            return vec![];
        }

        // Equal distribution
        let percentage = 100 / pane_count as u16;
        let mut constraints: Vec<Constraint> = (0..pane_count - 1)
            .map(|_| Constraint::Percentage(percentage))
            .collect();
        // Last pane takes remaining space
        constraints.push(Constraint::Min(0));
        constraints
    }

    /// Render all panes from the multiplexer
    pub fn render(&self, frame: &mut Frame, mux: &Mux) {
        let area = frame.area();
        let panes: Vec<&Pane> = mux.panes().collect();

        if panes.is_empty() {
            return;
        }

        // Calculate layout
        let constraints = self.calculate_constraints(panes.len());
        let direction = match self.direction {
            LayoutDirection::Horizontal => Direction::Horizontal,
            LayoutDirection::Vertical => Direction::Vertical,
        };

        let chunks = Layout::default()
            .direction(direction)
            .constraints(constraints)
            .split(area);

        // Render each pane
        for (pane, chunk) in panes.iter().zip(chunks.iter()) {
            self.render_pane(frame, pane, *chunk);
        }
    }

    /// Render a single pane
    fn render_pane(&self, frame: &mut Frame, pane: &Pane, area: Rect) {
        // Get pane content as ratatui lines
        let lines = pane.viewport_as_lines().unwrap_or_default();

        let text = Text::from(lines);
        let mut paragraph = Paragraph::new(text);

        if self.border {
            let border_color = if pane.is_focused() {
                // Parse hex color if provided, otherwise use green
                pane.color()
                    .and_then(parse_hex_color)
                    .unwrap_or(Color::Green)
            } else {
                Color::DarkGray
            };

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(pane.title().to_string());

            paragraph = paragraph.block(block);
        }

        frame.render_widget(paragraph, area);
    }

    /// Render a single pane to a specific area (for custom layouts)
    pub fn render_pane_to(&self, frame: &mut Frame, pane: &Pane, area: Rect) {
        self.render_pane(frame, pane, area);
    }
}

/// Parse a hex color string (#RRGGBB or RRGGBB) to a ratatui Color
fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;

    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use crate::render::*;

    #[test]
    fn test_calculate_constraints_horizontal() {
        let renderer = Renderer::new();
        let constraints = renderer.calculate_constraints(2);
        assert_eq!(constraints.len(), 2);
    }

    #[test]
    fn test_calculate_constraints_empty() {
        let renderer = Renderer::new();
        let constraints = renderer.calculate_constraints(0);
        assert!(constraints.is_empty());
    }

    #[test]
    fn test_parse_hex_color() {
        assert!(matches!(
            parse_hex_color("#FF0000"),
            Some(Color::Rgb(255, 0, 0))
        ));
        assert!(matches!(
            parse_hex_color("00FF00"),
            Some(Color::Rgb(0, 255, 0))
        ));
        assert!(parse_hex_color("#FFF").is_none()); // Too short
    }

    #[test]
    fn test_layout_direction_default() {
        let renderer = Renderer::new();
        assert_eq!(renderer.direction, LayoutDirection::Horizontal);
    }
}
