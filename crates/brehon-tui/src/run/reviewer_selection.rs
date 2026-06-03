//! Reviewer selection state: capture, apply, and focus helpers.

use std::collections::HashMap;

use brehon_mux::Mux;

use super::types::ReviewerPanel;

#[derive(Debug, Default)]
pub(crate) struct ReviewerSelectionState {
    pub(crate) selected_panel_name: Option<String>,
    pub(crate) selected_member_by_panel: HashMap<String, String>,
}

pub(crate) fn capture_reviewer_selection_state(
    panels: &[ReviewerPanel],
    selected_panel: usize,
    selected_member: &[usize],
    state: &mut ReviewerSelectionState,
) {
    let panel_names: std::collections::HashSet<&str> =
        panels.iter().map(|panel| panel.name.as_str()).collect();
    state
        .selected_member_by_panel
        .retain(|panel_name, _| panel_names.contains(panel_name.as_str()));

    for (panel_index, panel) in panels.iter().enumerate() {
        let member_index = selected_member.get(panel_index).copied().unwrap_or(0);
        if let Some(member_id) = panel.members.get(member_index) {
            state
                .selected_member_by_panel
                .insert(panel.name.clone(), member_id.clone());
        }
    }

    state.selected_panel_name = panels.get(selected_panel).map(|panel| panel.name.clone());
}

pub(crate) fn apply_reviewer_selection_state(
    panels: &[ReviewerPanel],
    state: &mut ReviewerSelectionState,
    selected_panel: &mut usize,
    selected_member: &mut Vec<usize>,
) {
    selected_member.resize(panels.len(), 0);
    if panels.is_empty() {
        selected_member.clear();
        *selected_panel = 0;
        state.selected_panel_name = None;
        state.selected_member_by_panel.clear();
        return;
    }

    let preferred_member = state
        .selected_panel_name
        .as_ref()
        .and_then(|panel_name| state.selected_member_by_panel.get(panel_name))
        .cloned();

    let mut resolved_panel_index = state
        .selected_panel_name
        .as_ref()
        .and_then(|panel_name| panels.iter().position(|panel| panel.name == *panel_name));

    if let Some(member_id) = preferred_member.clone() {
        let selected_panel_missing_member = resolved_panel_index.is_some_and(|panel_index| {
            !panels[panel_index]
                .members
                .iter()
                .any(|member| member == &member_id)
        });
        if resolved_panel_index.is_none() || selected_panel_missing_member {
            resolved_panel_index = panels
                .iter()
                .position(|panel| panel.members.iter().any(|member| member == &member_id));
        }
    }

    let resolved_panel_index = resolved_panel_index.unwrap_or(0);
    *selected_panel = resolved_panel_index;
    state.selected_panel_name = Some(panels[resolved_panel_index].name.clone());

    if let Some(member_id) = preferred_member {
        state
            .selected_member_by_panel
            .insert(panels[resolved_panel_index].name.clone(), member_id);
    }

    for (panel_index, panel) in panels.iter().enumerate() {
        let remembered_member = state
            .selected_member_by_panel
            .get(&panel.name)
            .and_then(|member_id| panel.members.iter().position(|member| member == member_id))
            .unwrap_or(0);
        selected_member[panel_index] = remembered_member;

        if let Some(member_id) = panel.members.get(remembered_member) {
            state
                .selected_member_by_panel
                .insert(panel.name.clone(), member_id.clone());
        } else {
            state.selected_member_by_panel.remove(&panel.name);
        }
    }
}

pub(crate) fn focus_current_reviewer(
    mux: &mut Mux,
    panels: &[ReviewerPanel],
    selected_panel: usize,
    selected_member: &[usize],
) {
    if let Some(panel) = panels.get(selected_panel) {
        let mi = selected_member.get(selected_panel).copied().unwrap_or(0);
        if let Some(id) = panel.members.get(mi) {
            mux.focus(id);
        }
    }
}
