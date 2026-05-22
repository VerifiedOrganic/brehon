//! Session and dashboard refresh logic.

use std::sync::{Arc, Mutex};

use brehon_mux::{Mux, PaneKind, PaneState};

use super::dashboard::read_task_files;
use super::helpers::read_live_reviewer_panels;
use super::recovery::{
    detect_shared_root_mutation, push_dashboard_event, sync_worker_task_contexts,
};
use super::session::{read_session_files, refresh_session_file};
use super::types::{AgentInfo, DashboardData, ReviewerPanel, TaskInfo};
use super::{
    apply_reviewer_selection_state, capture_reviewer_selection_state, ReviewerSelectionState,
};

#[derive(Clone)]
pub(crate) struct SessionRefreshEntry {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) session_id: String,
    pub(crate) agent_type: String,
}

pub(crate) struct DashboardRefreshSnapshot {
    pub(crate) tasks: Vec<TaskInfo>,
    pub(crate) sessions: std::collections::HashMap<String, (String, String, String)>,
    pub(crate) panels: Vec<ReviewerPanel>,
    pub(crate) shared_root_issue: Option<String>,
}

pub(crate) fn collect_session_refresh_entries(mux: &Mux) -> Vec<SessionRefreshEntry> {
    mux.panes()
        .filter(|pane| {
            !pane.has_exited() && !matches!(pane.pane_state(), Some(PaneState::Dead { .. }))
        })
        .filter_map(|pane| {
            let session_id = pane.agent_session_id()?;
            let role = match pane.kind() {
                PaneKind::Worker => "worker",
                PaneKind::Supervisor => "supervisor",
                PaneKind::Reviewer => "reviewer",
                PaneKind::Advisor => "advisor",
                PaneKind::Research => "research",
                PaneKind::Director => "director",
                PaneKind::Shell => "shell",
            };
            Some(SessionRefreshEntry {
                name: pane.id().to_string(),
                role: role.to_string(),
                session_id: session_id.to_string(),
                agent_type: pane
                    .configured_agent_type()
                    .unwrap_or(pane.cli_type().name())
                    .to_string(),
            })
        })
        .collect()
}

pub(crate) fn collect_dashboard_refresh(
    brehon_root: &std::path::Path,
    session_entries: &[SessionRefreshEntry],
    fallback_panels: &[ReviewerPanel],
) -> DashboardRefreshSnapshot {
    for entry in session_entries {
        refresh_session_file(
            brehon_root,
            &entry.name,
            &entry.role,
            &entry.session_id,
            &entry.agent_type,
        );
    }

    DashboardRefreshSnapshot {
        tasks: read_task_files(brehon_root),
        sessions: read_session_files(brehon_root),
        panels: read_live_reviewer_panels(brehon_root, fallback_panels),
        shared_root_issue: detect_shared_root_mutation(brehon_root),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_dashboard_refresh_snapshot(
    mux: &mut Mux,
    dashboard_data: &Arc<Mutex<DashboardData>>,
    panels: &mut Vec<ReviewerPanel>,
    selected_panel: &mut usize,
    selected_member: &mut Vec<usize>,
    reviewer_selection: &mut ReviewerSelectionState,
    last_shared_root_issue: &mut Option<String>,
    snapshot: DashboardRefreshSnapshot,
) {
    let DashboardRefreshSnapshot {
        tasks,
        sessions,
        panels: refreshed_panels,
        shared_root_issue,
    } = snapshot;

    capture_reviewer_selection_state(panels, *selected_panel, selected_member, reviewer_selection);
    *panels = refreshed_panels;
    if shared_root_issue != *last_shared_root_issue {
        if let Some(issue) = &shared_root_issue {
            push_dashboard_event(dashboard_data, issue.clone());
            tracing::error!("{issue}");
        }
        *last_shared_root_issue = shared_root_issue;
    }
    apply_reviewer_selection_state(panels, reviewer_selection, selected_panel, selected_member);
    sync_worker_task_contexts(mux, &tasks, &sessions);

    let mut data = dashboard_data.lock().unwrap();
    data.tasks = tasks;
    for pane in mux.panes() {
        let name = pane.id().to_string();
        let role = match pane.kind() {
            PaneKind::Worker => "worker",
            PaneKind::Supervisor => "supervisor",
            PaneKind::Reviewer => "reviewer",
            PaneKind::Advisor => "advisor",
            PaneKind::Research => "research",
            PaneKind::Director => "director",
            PaneKind::Shell => "shell",
        };
        let (session_id, last_seen_at) = if let Some((_, sid, seen)) = sessions.get(&name) {
            (Some(sid.clone()), Some(seen.clone()))
        } else {
            (None, None)
        };
        if let Some(existing) = data.agents.iter_mut().find(|agent| agent.name == name) {
            existing.session_id = session_id;
            existing.last_seen_at = last_seen_at;
        } else {
            data.agents.push(AgentInfo {
                name,
                role: role.to_string(),
                cli: pane.cli_type().name().to_string(),
                session_id,
                last_seen_at,
            });
        }
    }
}
