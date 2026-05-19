//! Brehon TUI — terminal multiplexer interface.
//!
//! Split layout: always-visible supervisor on the right,
//! tabbed workers/reviewers on the left with panel grouping.

pub mod components;
pub mod theme;

mod run;

pub use run::{
    run_dashboard_tui, run_tui, run_tui_with_panels, run_tui_with_panels_and_runtime_commands,
    AgentInfo, DashboardData, EventInfo, ReviewerPanel, TaskInfo,
};
