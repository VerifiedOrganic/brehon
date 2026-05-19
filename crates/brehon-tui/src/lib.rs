//! Brehon TUI — terminal multiplexer interface.
//!
//! Split layout: always-visible supervisor on the right,
//! tabbed workers/reviewers on the left with panel grouping.

// TUI render and key-handling functions naturally take many state references;
// breaking them into argument structs would obscure the call sites without
// changing behaviour.
#![allow(clippy::too_many_arguments)]

pub mod components;
pub mod theme;

mod run;

pub use run::research::ProjectConfigLoader;
pub use run::{
    no_project_config_loader, run_dashboard_tui, run_tui, run_tui_with_panels,
    run_tui_with_panels_and_runtime_commands, AgentInfo, DashboardData, EventInfo, ReviewerPanel,
    TaskInfo,
};
