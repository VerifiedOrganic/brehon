use std::io::ErrorKind;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::helpers::brehon_root;
use crate::tools::agent::{session_is_live, session_matches_current_runtime};

pub(crate) const IMPLICIT_PANEL_ID: &str = "default-panel";

/// Agent info returned from session file discovery.
pub(super) struct AgentInfo {
    pub(super) name: String,
    pub(super) agent_type: String,
}

/// A single member slot within a review panel lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelLeaseMember {
    pub slot_agent: String,
    pub reviewer: String,
}

/// Persisted state of a review panel lease, tracking which reviewers are assigned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelLeaseState {
    pub panel_id: String,
    pub task_id: String,
    pub review_id: String,
    pub round: u32,
    pub members: Vec<PanelLeaseMember>,
    pub leased_at: String,
    pub updated_at: String,
}

impl PanelLeaseState {
    pub(crate) fn panel(&self) -> Vec<String> {
        self.members
            .iter()
            .map(|member| member.reviewer.clone())
            .collect()
    }
}

/// Persisted physical reviewer seats for a configured panel in the current run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelSeatState {
    pub panel_id: String,
    pub members: Vec<PanelLeaseMember>,
    #[serde(default)]
    pub updated_at: String,
}

pub(crate) fn review_panels_dir() -> Option<PathBuf> {
    brehon_root().map(|root| root.join("runtime").join("review-panels"))
}

pub(crate) fn review_panel_seats_dir() -> Option<PathBuf> {
    brehon_root().map(|root| root.join("runtime").join("review-panel-seats"))
}

fn sanitize_panel_lease_key(key: &str) -> String {
    let mut filename = String::new();
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            filename.push(ch);
        } else {
            filename.push('_');
        }
    }
    filename
}

pub(crate) fn panel_lease_filename(task_id: &str) -> String {
    let filename = sanitize_panel_lease_key(task_id);
    format!("{filename}.json")
}

pub(crate) fn panel_seat_filename(panel_id: &str) -> String {
    let filename = sanitize_panel_lease_key(panel_id);
    format!("{filename}.json")
}

pub(crate) fn read_panel_seat(panel_id: &str) -> Option<PanelSeatState> {
    let dir = review_panel_seats_dir()?;
    let path = dir.join(panel_seat_filename(panel_id));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn read_all_panel_leases() -> Vec<PanelLeaseState> {
    let Some(dir) = review_panels_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let content = std::fs::read_to_string(entry.path()).ok()?;
            serde_json::from_str(&content).ok()
        })
        .collect()
}

pub(crate) fn find_panel_lease_by_task(task_id: &str) -> Option<PanelLeaseState> {
    read_all_panel_leases()
        .into_iter()
        .find(|lease| lease.task_id == task_id)
}

pub(crate) fn write_panel_lease(lease: &PanelLeaseState) -> std::io::Result<()> {
    let dir = review_panels_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No review panels dir"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(panel_lease_filename(&lease.task_id));
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(lease).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)
}

pub(crate) fn delete_panel_lease(task_id: &str) -> std::io::Result<()> {
    let Some(dir) = review_panels_dir() else {
        return Ok(());
    };

    let primary_path = dir.join(panel_lease_filename(task_id));
    match std::fs::remove_file(&primary_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path == primary_path || path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(lease) = serde_json::from_str::<PanelLeaseState>(&content) else {
            continue;
        };
        if lease.task_id != task_id {
            continue;
        }

        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

pub(crate) fn release_panel_lease_for_task(task_id: &str) -> Result<Option<String>, String> {
    let Some(lease) = find_panel_lease_by_task(task_id) else {
        return Ok(None);
    };
    delete_panel_lease(task_id).map_err(|err| {
        format!(
            "Task {task_id} reached a terminal state, but Brehon failed to release panel '{}' : {err}",
            lease.panel_id
        )
    })?;
    Ok(Some(lease.panel_id))
}

pub(super) fn build_panel(
    default_reviewers: &[String],
    live_reviewers: &[AgentInfo],
    target_size: usize,
) -> Vec<String> {
    let mut panel: Vec<String> = Vec::new();
    let mut matched_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for cfg_type in default_reviewers.iter().take(target_size) {
        for agent in live_reviewers {
            if agent.agent_type == *cfg_type && !matched_names.contains(&agent.name) {
                panel.push(agent.name.clone());
                matched_names.insert(agent.name.clone());
                break;
            }
        }
    }

    for agent in live_reviewers {
        if panel.len() >= target_size {
            break;
        }
        if !matched_names.contains(&agent.name) {
            panel.push(agent.name.clone());
            matched_names.insert(agent.name.clone());
        }
    }

    panel
}

pub(super) fn build_full_council_panel(
    default_reviewers: &[String],
    live_reviewers: &[AgentInfo],
) -> Vec<String> {
    if default_reviewers.is_empty() {
        let mut panel: Vec<String> = live_reviewers
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        panel.sort();
        panel.dedup();
        return panel;
    }

    let mut panel: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for cfg_type in default_reviewers {
        let mut matching: Vec<String> = live_reviewers
            .iter()
            .filter(|agent| agent.agent_type == *cfg_type)
            .map(|agent| agent.name.clone())
            .collect();
        matching.sort();

        for reviewer in matching {
            if seen.insert(reviewer.clone()) {
                panel.push(reviewer);
            }
        }
    }

    panel
}

/// Find all agents with a given role from session files.
pub(super) fn find_agents_by_role(role: &str) -> Vec<String> {
    find_agents_by_role_with_type(role)
        .into_iter()
        .map(|a| a.name)
        .collect()
}

/// Like `find_agents_by_role` but also returns each agent's type from session file.
pub(super) fn find_agents_by_role_with_type(role: &str) -> Vec<AgentInfo> {
    let Some(dir) = brehon_root().map(|r| r.join("runtime").join("sessions")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "json")
                && !e.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            let v: Value = serde_json::from_str(&content).ok()?;
            if v.get("role").and_then(|r| r.as_str()) == Some(role)
                && session_is_live(&v)
                && session_matches_current_runtime(&v)
            {
                let name = v.get("name").and_then(|n| n.as_str())?.to_string();
                if agent_is_quarantined(&name) {
                    return None;
                }
                let agent_type = v
                    .get("agent_type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(AgentInfo { name, agent_type })
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn agent_health_path(agent_name: &str) -> Option<PathBuf> {
    let mut file_name = String::new();
    for ch in agent_name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            file_name.push(ch);
        } else {
            file_name.push('_');
        }
    }
    brehon_root().map(|root| {
        root.join("runtime")
            .join("agent-health")
            .join(format!("{file_name}.json"))
    })
}

/// Records a reviewer swap within a panel (removed reviewer replaced by another).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelReviewerReplacement {
    pub removed: String,
    pub replaced_with: String,
}

pub(super) fn agent_is_quarantined(agent_name: &str) -> bool {
    let Some(path) = agent_health_path(agent_name) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    value.get("status").and_then(|status| status.as_str()) == Some("unavailable")
}
