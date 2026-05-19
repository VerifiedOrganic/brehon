use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use brehon_types::{config::ReviewLeaseMode, BrehonConfig};
use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::IMPLICIT_PANEL_ID;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RuntimePanelLeaseMember {
    pub(crate) slot_agent: String,
    pub(crate) reviewer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RuntimePanelLeaseState {
    pub(crate) panel_id: String,
    pub(crate) task_id: String,
    pub(crate) review_id: String,
    pub(crate) round: u32,
    pub(crate) members: Vec<RuntimePanelLeaseMember>,
    pub(crate) leased_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RuntimeReviewState {
    pub(crate) task_id: String,
    pub(crate) status: String,
    pub(crate) current_round: u32,
    #[serde(default = "default_runtime_cycle_start_round")]
    pub(crate) cycle_start_round: u32,
    pub(crate) current_review_id: String,
    pub(crate) max_rounds: u8,
    #[serde(default = "default_runtime_panel_id")]
    pub(crate) panel_id: String,
    #[serde(default = "default_runtime_panel_mode")]
    pub(crate) panel_mode: String,
    pub(crate) panel: Vec<String>,
    pub(crate) submissions_received: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RuntimeReviewRequest {
    pub(crate) task_id: String,
    pub(crate) review_id: String,
    pub(crate) requested_by: String,
    pub(crate) requested_at: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) commit: String,
    pub(crate) context: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedReviewPanel {
    pub(crate) panel_id: String,
    pub(crate) members: Vec<RuntimePanelLeaseMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimePanelSeatState {
    panel_id: String,
    members: Vec<RuntimePanelLeaseMember>,
    updated_at: String,
}

pub(crate) fn default_runtime_panel_id() -> String {
    IMPLICIT_PANEL_ID.to_string()
}

pub(crate) fn default_runtime_panel_mode() -> String {
    "full_council".to_string()
}

pub(crate) fn default_runtime_cycle_start_round() -> u32 {
    1
}

pub(crate) fn build_planned_review_panel_seats(
    config: &BrehonConfig,
    pool_reviewer_names: &[Vec<String>],
) -> Vec<PlannedReviewPanel> {
    if !config.review.panels.is_empty() {
        let mut names_by_agent: HashMap<String, Vec<String>> = HashMap::new();
        for (pool, names) in config
            .roles
            .reviewers
            .iter()
            .zip(pool_reviewer_names.iter())
        {
            names_by_agent.insert(pool.lane.clone(), names.clone());
        }

        let mut offsets: HashMap<String, usize> = HashMap::new();
        let mut planned = Vec::new();
        for panel in &config.review.panels {
            let mut members = Vec::new();
            let mut complete = true;
            for slot_agent in &panel.reviewers {
                let offset = offsets.entry(slot_agent.clone()).or_insert(0);
                let Some(names) = names_by_agent.get(slot_agent) else {
                    complete = false;
                    break;
                };
                let Some(name) = names.get(*offset) else {
                    complete = false;
                    break;
                };
                members.push(RuntimePanelLeaseMember {
                    slot_agent: slot_agent.clone(),
                    reviewer: name.clone(),
                });
                *offset += 1;
            }
            if complete && !members.is_empty() {
                planned.push(PlannedReviewPanel {
                    panel_id: panel.id.clone(),
                    members,
                });
            }
        }
        return planned;
    }

    let max_per_pool = pool_reviewer_names
        .iter()
        .map(|pool| pool.len())
        .max()
        .unwrap_or(0);
    let mut planned = Vec::new();
    for panel_idx in 0..max_per_pool {
        let mut members = Vec::new();
        for (pool, pool_names) in config
            .roles
            .reviewers
            .iter()
            .zip(pool_reviewer_names.iter())
        {
            if let Some(name) = pool_names.get(panel_idx) {
                members.push(RuntimePanelLeaseMember {
                    slot_agent: pool.lane.clone(),
                    reviewer: name.clone(),
                });
            }
        }
        if !members.is_empty() {
            planned.push(PlannedReviewPanel {
                panel_id: if panel_idx == 0 {
                    IMPLICIT_PANEL_ID.to_string()
                } else {
                    format!("Panel {}", panel_idx + 1)
                },
                members,
            });
        }
    }
    planned
}

pub(crate) fn runtime_panel_lease_filename(task_id: &str) -> String {
    let mut filename = String::new();
    for ch in task_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            filename.push(ch);
        } else {
            filename.push('_');
        }
    }
    format!("{filename}.json")
}

fn runtime_panel_seat_filename(panel_id: &str) -> String {
    let mut filename = String::new();
    for ch in panel_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            filename.push(ch);
        } else {
            filename.push('_');
        }
    }
    format!("{filename}.json")
}

fn write_runtime_panel_seats(
    brehon_root: &Path,
    planned_panels: &[PlannedReviewPanel],
    now: &str,
) -> Result<()> {
    let panel_seats_dir = brehon_root.join("runtime").join("review-panel-seats");
    std::fs::create_dir_all(&panel_seats_dir)?;

    let planned_ids: HashSet<String> = planned_panels
        .iter()
        .map(|panel| panel.panel_id.clone())
        .collect();

    if let Ok(entries) = std::fs::read_dir(&panel_seats_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(panel_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !planned_ids.contains(panel_id) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    for panel in planned_panels {
        let seat = RuntimePanelSeatState {
            panel_id: panel.panel_id.clone(),
            members: panel.members.clone(),
            updated_at: now.to_string(),
        };
        let path = panel_seats_dir.join(runtime_panel_seat_filename(&panel.panel_id));
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&seat)?)?;
        std::fs::rename(&tmp, path)?;
    }

    Ok(())
}

pub(crate) fn runtime_task_status(brehon_root: &Path, task_id: &str) -> Option<String> {
    let path = brehon_root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value
        .get("status")
        .and_then(|status| status.as_str())
        .map(str::to_string)
}

pub(crate) fn runtime_task_path(brehon_root: &Path, task_id: &str) -> PathBuf {
    brehon_root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"))
}

pub(crate) fn reconcile_task_gate_from_review_state(
    brehon_root: &Path,
    task_id: &str,
    review_state: &RuntimeReviewState,
    now: &str,
) -> Result<()> {
    let path = runtime_task_path(brehon_root, task_id);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut value: serde_json::Value = serde_json::from_str(&content)?;
    let current_status = value
        .get("status")
        .and_then(|status| status.as_str())
        .unwrap_or("pending");
    if task_is_terminal_status(current_status) {
        return Ok(());
    }

    let desired_status = match review_state.status.as_str() {
        "collecting" => Some("in_review"),
        "approved" => Some("approved"),
        "changes_requested" | "rejected" | "escalated" | "released" => Some("changes_requested"),
        _ => None,
    };

    let mut changed = false;
    if let Some(desired_status) = desired_status {
        if current_status != desired_status {
            value["status"] = serde_json::json!(desired_status);
            changed = true;
        }

        if matches!(
            desired_status,
            "in_review" | "changes_requested" | "approved"
        ) && value
            .get("assignee")
            .and_then(|assignee| assignee.as_str())
            .is_none()
        {
            if let Some(review_owner) = value
                .get("review_owner")
                .and_then(|review_owner| review_owner.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                value["assignee"] = serde_json::json!(review_owner);
                changed = true;
            }
        }
    }

    if changed {
        value["updated_at"] = serde_json::json!(now);
        std::fs::write(&path, serde_json::to_string_pretty(&value)?)?;
    }

    Ok(())
}

pub(crate) fn recover_orphaned_review_gate_task(path: &Path, now: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut value: serde_json::Value = serde_json::from_str(&content)?;
    let current_status = value
        .get("status")
        .and_then(|status| status.as_str())
        .unwrap_or("pending");
    if current_status != "in_review" {
        return Ok(());
    }

    let blockers = value
        .get("blockers")
        .and_then(|blockers| blockers.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let review_feedback_outcome = value
        .get("review_feedback")
        .and_then(|feedback| feedback.get("outcome"))
        .and_then(|outcome| outcome.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let should_restore_revision = review_feedback_outcome == "changes_requested"
        || blockers.contains("does not integrate cleanly")
        || blockers.contains("checkpoint again")
        || blockers.contains("re-request review")
        || blockers.contains("resubmit");

    if should_restore_revision {
        value["status"] = serde_json::json!("changes_requested");
        if value
            .get("assignee")
            .and_then(|assignee| assignee.as_str())
            .is_none()
        {
            if let Some(review_owner) = value
                .get("review_owner")
                .and_then(|review_owner| review_owner.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                value["assignee"] = serde_json::json!(review_owner);
            }
        }
    } else {
        value["status"] = serde_json::json!("review_ready");
        if value
            .get("assignee")
            .and_then(|assignee| assignee.as_str())
            .is_none()
        {
            if let Some(review_owner) = value
                .get("review_owner")
                .and_then(|review_owner| review_owner.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                value["assignee"] = serde_json::json!(review_owner);
            }
        }
    }

    value["updated_at"] = serde_json::json!(now);
    std::fs::write(path, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

pub(crate) fn task_is_terminal_status(status: &str) -> bool {
    brehon_types::task::normalize_task_status(status)
        .is_some_and(|status| matches!(status, "merged" | "closed"))
}

pub(crate) fn runtime_review_state_path(brehon_root: &Path, task_id: &str) -> PathBuf {
    brehon_root
        .join("runtime")
        .join("reviews")
        .join(task_id)
        .join("state.json")
}

pub(crate) fn delete_runtime_review_state(brehon_root: &Path, task_id: &str) -> Result<()> {
    let path = runtime_review_state_path(brehon_root, task_id);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn review_state_is_superseded_by_task_status(
    task_status: &str,
    review_state: &RuntimeReviewState,
) -> bool {
    let normalized = brehon_types::task::normalize_task_status(task_status);
    match review_state.status.as_str() {
        "changes_requested" | "released" | "rejected" | "escalated" => matches!(
            normalized,
            Some("review_ready" | "in_progress" | "assigned")
        ),
        "approved" => matches!(
            normalized,
            Some("review_ready" | "in_progress" | "assigned" | "changes_requested")
        ),
        _ => false,
    }
}

pub(crate) fn runtime_review_prompt(
    state: &RuntimeReviewState,
    request: &RuntimeReviewRequest,
    reviewer: &str,
) -> String {
    format!(
        "Review request {} for task {}: {}\n\
         Panel: {}\n\
         Round: {}\n\
         This review remained active across an Brehon restart and has been re-seated on the current reviewer panel.\n\
         {}\
         {}\
         {}\
         \n\
         Review for: correctness, security, performance, concurrency, error handling, and maintainability.\n\
         \n\
         Submit your review (IMPORTANT: include reviewer={}):\n\
           verification action=submit_review review_id={} reviewer={} \
         score=<1-10> verdict=<approved|needs_revision|rejected> \
         summary=\"Your review\" findings='[{{\"description\":\"...\", \
         \"file\":\"...\", \"line\":42, \"severity\":\"blocking|suggestion|nitpick\", \
         \"suggestion\":\"optional\"}}]'",
        state.current_review_id,
        state.task_id,
        request.title,
        state.panel_id,
        state.current_round,
        if request.description.trim().is_empty() {
            String::new()
        } else {
            format!("Description: {}\n", request.description)
        },
        if request.commit.trim().is_empty() {
            String::new()
        } else {
            format!("Commit: {}\n", request.commit)
        },
        if request.context.trim().is_empty() {
            String::new()
        } else {
            format!("Context: {}\n", request.context)
        },
        reviewer,
        state.current_review_id,
        reviewer,
    )
}

pub(crate) fn enqueue_runtime_prompt(
    prompt_queue_dir: &Path,
    target: &str,
    from: &str,
    message: &str,
) -> Result<()> {
    std::fs::create_dir_all(prompt_queue_dir)?;

    let file_name = format!(
        "{:020}-{}.prompt",
        chrono::Utc::now().timestamp_millis(),
        uuid::Uuid::new_v4()
    );
    let final_path = prompt_queue_dir.join(&file_name);
    let temp_path = prompt_queue_dir.join(format!(".{file_name}.tmp"));
    let session_name = prompt_queue_dir
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "prompt-queue" && *value != "_legacy")
        .map(str::to_string);
    let payload = serde_json::json!({
        "target": target,
        "from": from,
        "message": message,
        "session_name": session_name,
    });
    let encoded = serde_json::to_string(&payload)?;
    std::fs::write(&temp_path, encoded)?;
    std::fs::rename(&temp_path, &final_path)?;
    Ok(())
}

pub(crate) fn runtime_prompt_queue_dir(brehon_root: &Path) -> PathBuf {
    let base = brehon_root.join("runtime").join("prompt-queue");
    let session_name =
        std::fs::read_to_string(brehon_root.join("runtime").join("current-session.json"))
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|value| {
                value
                    .get("session_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

    match session_name {
        Some(session_name) => base.join(session_name),
        None => base,
    }
}

pub(crate) fn migrate_current_round_submission_aliases(
    round_dir: &Path,
    rename_map: &HashMap<String, String>,
) -> Result<()> {
    if rename_map.is_empty() || !round_dir.is_dir() {
        return Ok(());
    }

    let entries = match std::fs::read_dir(round_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        if matches!(file_name.as_str(), "request.json" | "consolidated.json")
            || file_name.starts_with('.')
        {
            continue;
        }
        let Some(old_reviewer) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(new_reviewer) = rename_map.get(old_reviewer) else {
            continue;
        };
        if new_reviewer == old_reviewer {
            continue;
        }

        let mut submission: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        if submission
            .get("reviewer")
            .and_then(|reviewer| reviewer.as_str())
            == Some(old_reviewer)
        {
            submission["reviewer"] = serde_json::json!(new_reviewer);
        }

        let new_path = round_dir.join(format!("{new_reviewer}.json"));
        if !new_path.exists() {
            let temp_path = round_dir.join(format!(".{new_reviewer}.tmp"));
            std::fs::write(&temp_path, serde_json::to_string_pretty(&submission)?)?;
            std::fs::rename(&temp_path, &new_path)?;
        }
        let _ = std::fs::remove_file(&path);
    }

    Ok(())
}

pub(crate) fn reconcile_review_runtime_for_run(
    brehon_root: &Path,
    planned_panels: &[PlannedReviewPanel],
    supervisor_name: &str,
    config: &BrehonConfig,
) -> Result<()> {
    let reviews_dir = brehon_root.join("runtime").join("reviews");
    let review_panels_dir = brehon_root.join("runtime").join("review-panels");
    let prompt_queue_dir = runtime_prompt_queue_dir(brehon_root);
    let now = chrono::Utc::now().to_rfc3339();

    std::fs::create_dir_all(&reviews_dir)?;
    std::fs::create_dir_all(&review_panels_dir)?;
    std::fs::create_dir_all(&prompt_queue_dir)?;
    write_runtime_panel_seats(brehon_root, planned_panels, &now)?;

    let planned_by_id: HashMap<String, PlannedReviewPanel> = planned_panels
        .iter()
        .cloned()
        .map(|panel| (panel.panel_id.clone(), panel))
        .collect();

    let mut lease_paths_by_task: HashMap<String, PathBuf> = HashMap::new();
    let mut leases_by_task: HashMap<String, RuntimePanelLeaseState> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(&review_panels_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(lease) = serde_json::from_str::<RuntimePanelLeaseState>(&content) else {
                continue;
            };
            lease_paths_by_task.insert(lease.task_id.clone(), path);
            leases_by_task.insert(lease.task_id.clone(), lease);
        }
    }

    let mut referenced_tasks = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(&reviews_dir) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }

            let state_path = entry.path().join("state.json");
            let Ok(content) = std::fs::read_to_string(&state_path) else {
                continue;
            };
            let Ok(mut state) = serde_json::from_str::<RuntimeReviewState>(&content) else {
                continue;
            };
            referenced_tasks.insert(state.task_id.clone());

            let current_task_status =
                runtime_task_status(brehon_root, &state.task_id).unwrap_or_default();
            if task_is_terminal_status(&current_task_status) {
                if let Some(path) = lease_paths_by_task.remove(&state.task_id) {
                    let _ = std::fs::remove_file(path);
                }
                leases_by_task.remove(&state.task_id);
                delete_runtime_review_state(brehon_root, &state.task_id)?;
                continue;
            }

            if config.review.lease_mode == ReviewLeaseMode::ShareAfterSubmit
                && config.review.panels.is_empty()
            {
                if let Some(path) = lease_paths_by_task.remove(&state.task_id) {
                    let _ = std::fs::remove_file(path);
                }
                leases_by_task.remove(&state.task_id);

                if state.status == "collecting" {
                    reconcile_task_gate_from_review_state(
                        brehon_root,
                        &state.task_id,
                        &state,
                        &now,
                    )?;
                    let _ = enqueue_runtime_prompt(
                        &prompt_queue_dir,
                        supervisor_name,
                        "review-runtime",
                        &format!(
                            "Preserved in-flight shared reviewer round {} for task {} during restart recovery. \
                             The task remains in_review; use review_status to inspect progress, reseat_panel if reviewers are gone, \
                             or reset_rounds only if you intentionally want to abandon this round.",
                            state.current_review_id, state.task_id
                        ),
                    );
                }
                continue;
            }

            if review_state_is_superseded_by_task_status(&current_task_status, &state) {
                if let Some(path) = lease_paths_by_task.remove(&state.task_id) {
                    let _ = std::fs::remove_file(path);
                }
                leases_by_task.remove(&state.task_id);
                delete_runtime_review_state(brehon_root, &state.task_id)?;
                continue;
            }

            reconcile_task_gate_from_review_state(brehon_root, &state.task_id, &state, &now)?;

            let Some(planned_panel) = planned_by_id.get(&state.panel_id) else {
                if state.status == "collecting" {
                    state.status = "released".to_string();
                    state.updated_at = now.clone();
                    std::fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;
                    reconcile_task_gate_from_review_state(
                        brehon_root,
                        &state.task_id,
                        &state,
                        &now,
                    )?;
                    if let Some(path) = lease_paths_by_task.remove(&state.task_id) {
                        let _ = std::fs::remove_file(path);
                    }
                    leases_by_task.remove(&state.task_id);
                    let _ = enqueue_runtime_prompt(
                        &prompt_queue_dir,
                        supervisor_name,
                        "review-runtime",
                        &format!(
                            "Released stale review {} for task {} because panel '{}' is no longer configured in this run. Request a fresh round when ready.",
                            state.current_review_id, state.task_id, state.panel_id
                        ),
                    );
                }
                continue;
            };

            let desired_panel_names: Vec<String> = planned_panel
                .members
                .iter()
                .map(|member| member.reviewer.clone())
                .collect();

            let rename_map: HashMap<String, String> = state
                .panel
                .iter()
                .cloned()
                .zip(desired_panel_names.iter().cloned())
                .filter(|(old, new)| old != new)
                .collect();

            let round_dir = entry.path().join(format!("round-{}", state.current_round));
            migrate_current_round_submission_aliases(&round_dir, &rename_map)?;

            let translated_submissions: HashSet<String> = state
                .submissions_received
                .iter()
                .map(|reviewer| {
                    rename_map
                        .get(reviewer)
                        .cloned()
                        .unwrap_or_else(|| reviewer.clone())
                })
                .collect();
            let reordered_submissions: Vec<String> = desired_panel_names
                .iter()
                .filter(|reviewer| translated_submissions.contains(*reviewer))
                .cloned()
                .collect();

            let mut state_changed = false;
            if state.panel != desired_panel_names {
                state.panel = desired_panel_names.clone();
                state_changed = true;
            }
            if state.submissions_received != reordered_submissions {
                state.submissions_received = reordered_submissions;
                state_changed = true;
            }

            let mut desired_lease = RuntimePanelLeaseState {
                panel_id: planned_panel.panel_id.clone(),
                task_id: state.task_id.clone(),
                review_id: state.current_review_id.clone(),
                round: state.current_round,
                members: planned_panel.members.clone(),
                leased_at: now.clone(),
                updated_at: now.clone(),
            };
            if let Some(existing) = leases_by_task.get(&state.task_id) {
                desired_lease.leased_at = existing.leased_at.clone();
            }

            let write_lease = match leases_by_task.get(&state.task_id) {
                Some(existing) => {
                    existing.panel_id != desired_lease.panel_id
                        || existing.review_id != desired_lease.review_id
                        || existing.round != desired_lease.round
                        || existing.members != desired_lease.members
                }
                None => true,
            };

            if write_lease {
                let path = review_panels_dir.join(runtime_panel_lease_filename(&state.task_id));
                std::fs::write(&path, serde_json::to_string_pretty(&desired_lease)?)?;
                lease_paths_by_task.insert(state.task_id.clone(), path);
                leases_by_task.insert(state.task_id.clone(), desired_lease);
            }

            if state_changed {
                state.updated_at = now.clone();
                std::fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;
            }

            if state.status == "collecting" {
                let request_path = round_dir.join("request.json");
                if let Ok(request_content) = std::fs::read_to_string(&request_path) {
                    if let Ok(request) =
                        serde_json::from_str::<RuntimeReviewRequest>(&request_content)
                    {
                        for reviewer in desired_panel_names
                            .iter()
                            .filter(|reviewer| !state.submissions_received.contains(*reviewer))
                        {
                            let _ = enqueue_runtime_prompt(
                                &prompt_queue_dir,
                                reviewer,
                                supervisor_name,
                                &runtime_review_prompt(&state, &request, reviewer),
                            );
                        }
                    }
                }
            }
        }
    }

    for (task_id, path) in lease_paths_by_task {
        if !referenced_tasks.contains(&task_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    let tasks_dir = brehon_root.join("runtime").join("tasks");
    if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(task_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if referenced_tasks.contains(task_id) {
                continue;
            }
            let _ = recover_orphaned_review_gate_task(&path, &now);
        }
    }

    Ok(())
}

pub(crate) fn build_reviewer_panels(
    config: &BrehonConfig,
    pool_reviewer_names: &[Vec<String>],
) -> Vec<brehon_tui::ReviewerPanel> {
    if !config.review.panels.is_empty() {
        let mut names_by_agent: HashMap<String, Vec<String>> = HashMap::new();
        for (pool, names) in config
            .roles
            .reviewers
            .iter()
            .zip(pool_reviewer_names.iter())
        {
            names_by_agent.insert(pool.lane.clone(), names.clone());
        }

        let mut offsets: HashMap<String, usize> = HashMap::new();
        let mut configured = Vec::new();
        for panel in &config.review.panels {
            let mut members = Vec::new();
            let mut complete = true;
            for slot_agent in &panel.reviewers {
                let offset = offsets.entry(slot_agent.clone()).or_insert(0);
                let Some(names) = names_by_agent.get(slot_agent) else {
                    complete = false;
                    break;
                };
                let Some(name) = names.get(*offset) else {
                    complete = false;
                    break;
                };
                members.push(name.clone());
                *offset += 1;
            }
            if complete && !members.is_empty() {
                configured.push(brehon_tui::ReviewerPanel {
                    name: panel.id.clone(),
                    members,
                });
            }
        }

        if !configured.is_empty() {
            return configured;
        }
    }

    let max_per_pool = pool_reviewer_names
        .iter()
        .map(|p| p.len())
        .max()
        .unwrap_or(0);
    let mut reviewer_panels: Vec<brehon_tui::ReviewerPanel> = Vec::new();
    for panel_idx in 0..max_per_pool {
        let mut members = Vec::new();
        for pool_names in pool_reviewer_names {
            if let Some(name) = pool_names.get(panel_idx) {
                members.push(name.clone());
            }
        }
        if !members.is_empty() {
            reviewer_panels.push(brehon_tui::ReviewerPanel {
                name: format!("Panel {}", panel_idx + 1),
                members,
            });
        }
    }
    reviewer_panels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_runtime_config() -> BrehonConfig {
        brehon_config::parse_defaults().unwrap()
    }

    #[test]
    fn test_build_reviewer_panels_uses_configured_panel_ids_and_members() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.roles.reviewers = vec![
            brehon_types::ReviewerPoolConfig {
                lane: "claude-reviewer".to_string(),
                model: Some(brehon_types::ModelConfig {
                    provider: "anthropic".to_string(),
                    name: "claude-sonnet-4-6".to_string(),
                }),
                reasoning_effort: None,
                system_prompt: None,
                min: 2,
                max: 2,
            },
            brehon_types::ReviewerPoolConfig {
                lane: "codex-reviewer".to_string(),
                model: Some(brehon_types::ModelConfig {
                    provider: "openai".to_string(),
                    name: "gpt-5.4".to_string(),
                }),
                reasoning_effort: None,
                system_prompt: None,
                min: 2,
                max: 2,
            },
        ];
        config.review.panels = vec![
            brehon_types::config::ReviewPanelConfig {
                id: "primary".to_string(),
                reviewers: vec!["claude-reviewer".to_string(), "codex-reviewer".to_string()],
            },
            brehon_types::config::ReviewPanelConfig {
                id: "secondary".to_string(),
                reviewers: vec!["claude-reviewer".to_string(), "codex-reviewer".to_string()],
            },
        ];

        let panels = build_reviewer_panels(
            &config,
            &[
                vec!["claude-a".to_string(), "claude-b".to_string()],
                vec!["codex-a".to_string(), "codex-b".to_string()],
            ],
        );

        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0].name, "primary");
        assert_eq!(panels[0].members, vec!["claude-a", "codex-a"]);
        assert_eq!(panels[1].name, "secondary");
        assert_eq!(panels[1].members, vec!["claude-b", "codex-b"]);
    }

    #[test]
    fn test_build_planned_review_panel_seats_uses_configured_slot_agents() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.roles.reviewers = vec![
            brehon_types::ReviewerPoolConfig {
                lane: "claude-reviewer".to_string(),
                model: Some(brehon_types::ModelConfig {
                    provider: "anthropic".to_string(),
                    name: "claude-opus-4-6".to_string(),
                }),
                reasoning_effort: None,
                system_prompt: None,
                min: 2,
                max: 2,
            },
            brehon_types::ReviewerPoolConfig {
                lane: "codex-reviewer".to_string(),
                model: Some(brehon_types::ModelConfig {
                    provider: "openai".to_string(),
                    name: "gpt-5.4".to_string(),
                }),
                reasoning_effort: None,
                system_prompt: None,
                min: 2,
                max: 2,
            },
            brehon_types::ReviewerPoolConfig {
                lane: "gemini-reviewer".to_string(),
                model: Some(brehon_types::ModelConfig {
                    provider: "google".to_string(),
                    name: "gemini-2.5-pro".to_string(),
                }),
                reasoning_effort: None,
                system_prompt: None,
                min: 2,
                max: 2,
            },
        ];
        config.review.panels = vec![
            brehon_types::config::ReviewPanelConfig {
                id: "primary".to_string(),
                reviewers: vec![
                    "claude-reviewer".to_string(),
                    "codex-reviewer".to_string(),
                    "gemini-reviewer".to_string(),
                ],
            },
            brehon_types::config::ReviewPanelConfig {
                id: "secondary".to_string(),
                reviewers: vec![
                    "claude-reviewer".to_string(),
                    "codex-reviewer".to_string(),
                    "gemini-reviewer".to_string(),
                ],
            },
        ];

        let planned = build_planned_review_panel_seats(
            &config,
            &[
                vec!["claude-a".to_string(), "claude-b".to_string()],
                vec!["codex-a".to_string(), "codex-b".to_string()],
                vec!["gemini-a".to_string(), "gemini-b".to_string()],
            ],
        );

        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].panel_id, "primary");
        assert_eq!(
            planned[0].members,
            vec![
                RuntimePanelLeaseMember {
                    slot_agent: "claude-reviewer".to_string(),
                    reviewer: "claude-a".to_string()
                },
                RuntimePanelLeaseMember {
                    slot_agent: "codex-reviewer".to_string(),
                    reviewer: "codex-a".to_string()
                },
                RuntimePanelLeaseMember {
                    slot_agent: "gemini-reviewer".to_string(),
                    reviewer: "gemini-a".to_string()
                }
            ]
        );
        assert_eq!(planned[1].panel_id, "secondary");
        assert_eq!(
            planned[1].members[0],
            RuntimePanelLeaseMember {
                slot_agent: "claude-reviewer".to_string(),
                reviewer: "claude-b".to_string()
            }
        );
    }

    #[test]
    fn test_reconcile_review_runtime_writes_physical_panel_seats() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(brehon_root.join("runtime").join("reviews")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panel-seats")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();
        std::fs::write(
            brehon_root
                .join("runtime")
                .join("review-panel-seats")
                .join("stale.json"),
            serde_json::json!({
                "panel_id": "stale",
                "members": [],
                "updated_at": "2026-04-09T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        let planned_panels = vec![PlannedReviewPanel {
            panel_id: "primary".to_string(),
            members: vec![
                RuntimePanelLeaseMember {
                    slot_agent: "claude-reviewer".to_string(),
                    reviewer: "claude-a".to_string(),
                },
                RuntimePanelLeaseMember {
                    slot_agent: "codex-reviewer".to_string(),
                    reviewer: "codex-a".to_string(),
                },
                RuntimePanelLeaseMember {
                    slot_agent: "gemini-reviewer".to_string(),
                    reviewer: "gemini-a".to_string(),
                },
            ],
        }];

        let config = default_runtime_config();
        reconcile_review_runtime_for_run(&brehon_root, &planned_panels, "supervisor", &config)
            .unwrap();

        let seat_path = brehon_root
            .join("runtime")
            .join("review-panel-seats")
            .join("primary.json");
        let seat: RuntimePanelSeatState =
            serde_json::from_str(&std::fs::read_to_string(seat_path).unwrap()).unwrap();
        assert_eq!(seat.panel_id, "primary");
        assert_eq!(seat.members, planned_panels[0].members);
        assert!(!brehon_root
            .join("runtime")
            .join("review-panel-seats")
            .join("stale.json")
            .exists());
    }

    #[test]
    fn test_reconcile_review_runtime_reseats_collecting_review_and_requeues_pending_reviewers() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let review_dir = brehon_root.join("runtime").join("reviews").join("T-1");
        let round_dir = review_dir.join("round-1");
        std::fs::create_dir_all(&round_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            brehon_root.join("runtime").join("tasks").join("T-1.json"),
            serde_json::json!({
                "task_id": "T-1",
                "title": "Task 1",
                "status": "in_review",
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&RuntimeReviewState {
                task_id: "T-1".to_string(),
                status: "collecting".to_string(),
                current_round: 1,
                cycle_start_round: 1,
                current_review_id: "REV-1".to_string(),
                max_rounds: 3,
                panel_id: "primary".to_string(),
                panel_mode: "configured_panel".to_string(),
                panel: vec![
                    "claude-old".to_string(),
                    "codex-old".to_string(),
                    "gemini-old".to_string(),
                ],
                submissions_received: vec!["claude-old".to_string()],
                created_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        std::fs::write(
            round_dir.join("request.json"),
            serde_json::to_string_pretty(&RuntimeReviewRequest {
                task_id: "T-1".to_string(),
                review_id: "REV-1".to_string(),
                requested_by: "supervisor".to_string(),
                requested_at: "2026-04-09T00:00:00Z".to_string(),
                title: "Task 1".to_string(),
                description: "Review me".to_string(),
                commit: "abc123".to_string(),
                context: "extra".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        std::fs::write(
            round_dir.join("claude-old.json"),
            serde_json::json!({
                "review_id": "REV-1",
                "reviewer": "claude-old",
                "round": 1,
                "score": 9,
                "verdict": "approved",
                "summary": "looks good",
                "findings": [],
                "submitted_at": "2026-04-09T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("review-panels")
                .join("T-1.json"),
            serde_json::to_string_pretty(&RuntimePanelLeaseState {
                panel_id: "primary".to_string(),
                task_id: "T-1".to_string(),
                review_id: "REV-1".to_string(),
                round: 1,
                members: vec![
                    RuntimePanelLeaseMember {
                        slot_agent: "claude-reviewer".to_string(),
                        reviewer: "claude-old".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "codex-reviewer".to_string(),
                        reviewer: "codex-old".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "gemini".to_string(),
                        reviewer: "gemini-old".to_string(),
                    },
                ],
                leased_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let planned_panels = vec![PlannedReviewPanel {
            panel_id: "primary".to_string(),
            members: vec![
                RuntimePanelLeaseMember {
                    slot_agent: "claude-reviewer".to_string(),
                    reviewer: "claude-new".to_string(),
                },
                RuntimePanelLeaseMember {
                    slot_agent: "codex-reviewer".to_string(),
                    reviewer: "codex-new".to_string(),
                },
                RuntimePanelLeaseMember {
                    slot_agent: "gemini".to_string(),
                    reviewer: "gemini-new".to_string(),
                },
            ],
        }];

        let config = default_runtime_config();
        reconcile_review_runtime_for_run(&brehon_root, &planned_panels, "supervisor", &config)
            .unwrap();

        let state: RuntimeReviewState =
            serde_json::from_str(&std::fs::read_to_string(review_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(
            state.panel,
            vec![
                "claude-new".to_string(),
                "codex-new".to_string(),
                "gemini-new".to_string()
            ]
        );
        assert_eq!(state.submissions_received, vec!["claude-new".to_string()]);

        let lease: RuntimePanelLeaseState = serde_json::from_str(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("review-panels")
                    .join("T-1.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            lease
                .members
                .iter()
                .map(|member| member.reviewer.clone())
                .collect::<Vec<_>>(),
            vec![
                "claude-new".to_string(),
                "codex-new".to_string(),
                "gemini-new".to_string()
            ]
        );
        assert!(round_dir.join("claude-new.json").exists());
        assert!(!round_dir.join("claude-old.json").exists());

        let queued_files = std::fs::read_dir(brehon_root.join("runtime").join("prompt-queue"))
            .unwrap()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(queued_files.len(), 2);
        let mut targets = queued_files
            .into_iter()
            .map(|entry| {
                let value: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
                value["target"].as_str().unwrap().to_string()
            })
            .collect::<Vec<_>>();
        targets.sort();
        assert_eq!(
            targets,
            vec!["codex-new".to_string(), "gemini-new".to_string()]
        );
    }

    #[test]
    fn test_reconcile_review_runtime_releases_collecting_review_on_missing_panel() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let review_dir = brehon_root.join("runtime").join("reviews").join("T-2");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            brehon_root.join("runtime").join("tasks").join("T-2.json"),
            serde_json::json!({
                "task_id": "T-2",
                "title": "Task 2",
                "status": "in_review",
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&RuntimeReviewState {
                task_id: "T-2".to_string(),
                status: "collecting".to_string(),
                current_round: 1,
                cycle_start_round: 1,
                current_review_id: "REV-2".to_string(),
                max_rounds: 3,
                panel_id: "default-panel".to_string(),
                panel_mode: "configured_panel".to_string(),
                panel: vec!["old-a".to_string(), "old-b".to_string()],
                submissions_received: Vec::new(),
                created_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("review-panels")
                .join("T-2.json"),
            serde_json::to_string_pretty(&RuntimePanelLeaseState {
                panel_id: "default-panel".to_string(),
                task_id: "T-2".to_string(),
                review_id: "REV-2".to_string(),
                round: 1,
                members: vec![
                    RuntimePanelLeaseMember {
                        slot_agent: "claude-reviewer".to_string(),
                        reviewer: "old-a".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "codex-reviewer".to_string(),
                        reviewer: "old-b".to_string(),
                    },
                ],
                leased_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let config = default_runtime_config();
        reconcile_review_runtime_for_run(
            &brehon_root,
            &[PlannedReviewPanel {
                panel_id: "primary".to_string(),
                members: vec![
                    RuntimePanelLeaseMember {
                        slot_agent: "claude-reviewer".to_string(),
                        reviewer: "claude-new".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "codex-reviewer".to_string(),
                        reviewer: "codex-new".to_string(),
                    },
                ],
            }],
            "supervisor",
            &config,
        )
        .unwrap();

        let state: RuntimeReviewState =
            serde_json::from_str(&std::fs::read_to_string(review_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(state.status, "released");
        assert!(!brehon_root
            .join("runtime")
            .join("review-panels")
            .join("T-2.json")
            .exists());
    }

    #[test]
    fn test_reconcile_review_runtime_updates_task_gate_from_non_collecting_review_state() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let review_dir = brehon_root.join("runtime").join("reviews").join("T-3");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            brehon_root.join("runtime").join("tasks").join("T-3.json"),
            serde_json::json!({
                "task_id": "T-3",
                "title": "Task 3",
                "status": "in_review",
                "assignee": serde_json::Value::Null,
                "review_owner": "worker-3",
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&RuntimeReviewState {
                task_id: "T-3".to_string(),
                status: "changes_requested".to_string(),
                current_round: 2,
                cycle_start_round: 1,
                current_review_id: "REV-3".to_string(),
                max_rounds: 3,
                panel_id: "primary".to_string(),
                panel_mode: "configured_panel".to_string(),
                panel: vec!["claude-a".to_string(), "codex-a".to_string()],
                submissions_received: vec!["claude-a".to_string(), "codex-a".to_string()],
                created_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let config = default_runtime_config();
        reconcile_review_runtime_for_run(
            &brehon_root,
            &[PlannedReviewPanel {
                panel_id: "primary".to_string(),
                members: vec![
                    RuntimePanelLeaseMember {
                        slot_agent: "claude-reviewer".to_string(),
                        reviewer: "claude-a".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "codex-reviewer".to_string(),
                        reviewer: "codex-a".to_string(),
                    },
                ],
            }],
            "supervisor",
            &config,
        )
        .unwrap();

        let task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(brehon_root.join("runtime").join("tasks").join("T-3.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(task["status"].as_str(), Some("changes_requested"));
        assert_eq!(task["assignee"].as_str(), Some("worker-3"));
    }

    #[test]
    fn test_reconcile_review_runtime_clears_superseded_changes_requested_state_after_task_returns_to_review_ready(
    ) {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let review_dir = brehon_root.join("runtime").join("reviews").join("T-3b");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            brehon_root.join("runtime").join("tasks").join("T-3b.json"),
            serde_json::json!({
                "task_id": "T-3b",
                "title": "Task 3b",
                "status": "review_ready",
                "assignee": serde_json::Value::Null,
                "review_owner": "worker-3",
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&RuntimeReviewState {
                task_id: "T-3b".to_string(),
                status: "changes_requested".to_string(),
                current_round: 2,
                cycle_start_round: 1,
                current_review_id: "REV-3b".to_string(),
                max_rounds: 3,
                panel_id: "primary".to_string(),
                panel_mode: "configured_panel".to_string(),
                panel: vec!["claude-a".to_string(), "codex-a".to_string()],
                submissions_received: vec!["claude-a".to_string(), "codex-a".to_string()],
                created_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let config = default_runtime_config();
        reconcile_review_runtime_for_run(
            &brehon_root,
            &[PlannedReviewPanel {
                panel_id: "primary".to_string(),
                members: vec![
                    RuntimePanelLeaseMember {
                        slot_agent: "claude-reviewer".to_string(),
                        reviewer: "claude-a".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "codex-reviewer".to_string(),
                        reviewer: "codex-a".to_string(),
                    },
                ],
            }],
            "supervisor",
            &config,
        )
        .unwrap();

        assert!(
            !review_dir.join("state.json").exists(),
            "superseded review state should be removed once task is back at review_ready"
        );
        let task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(brehon_root.join("runtime").join("tasks").join("T-3b.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(task["status"].as_str(), Some("review_ready"));
        assert!(task["assignee"].is_null());
        assert_eq!(task["review_owner"].as_str(), Some("worker-3"));
    }

    #[test]
    fn test_reconcile_review_runtime_clears_review_state_for_terminal_task() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let review_dir = brehon_root.join("runtime").join("reviews").join("T-term");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            brehon_root.join("runtime").join("tasks").join("T-term.json"),
            serde_json::json!({
                "task_id": "T-term",
                "title": "Terminal Task",
                "status": "closed",
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&RuntimeReviewState {
                task_id: "T-term".to_string(),
                status: "approved".to_string(),
                current_round: 1,
                cycle_start_round: 1,
                current_review_id: "REV-term".to_string(),
                max_rounds: 3,
                panel_id: "primary".to_string(),
                panel_mode: "configured_panel".to_string(),
                panel: vec!["claude-a".to_string(), "codex-a".to_string()],
                submissions_received: vec!["claude-a".to_string(), "codex-a".to_string()],
                created_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("review-panels")
                .join("T-term.json"),
            serde_json::to_string_pretty(&RuntimePanelLeaseState {
                panel_id: "primary".to_string(),
                task_id: "T-term".to_string(),
                review_id: "REV-term".to_string(),
                round: 1,
                members: vec![
                    RuntimePanelLeaseMember {
                        slot_agent: "claude-reviewer".to_string(),
                        reviewer: "claude-a".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "codex-reviewer".to_string(),
                        reviewer: "codex-a".to_string(),
                    },
                ],
                leased_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let config = default_runtime_config();
        reconcile_review_runtime_for_run(&brehon_root, &[], "supervisor", &config).unwrap();

        assert!(
            !review_dir.join("state.json").exists(),
            "terminal tasks should not retain active review state"
        );
        assert!(
            !brehon_root
                .join("runtime")
                .join("review-panels")
                .join("T-term.json")
                .exists(),
            "terminal tasks should release any persisted panel lease"
        );
    }

    #[test]
    fn test_reconcile_review_runtime_recovers_orphaned_in_review_task_without_review_state() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(brehon_root.join("runtime").join("reviews")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            brehon_root.join("runtime").join("tasks").join("T-4.json"),
            serde_json::json!({
                "task_id": "T-4",
                "title": "Task 4",
                "status": "in_review",
                "assignee": serde_json::Value::Null,
                "review_owner": "worker-4",
                "blockers": "Reviewed commit abc still does not integrate cleanly into epic/test. Checkpoint again and re-request review.",
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();

        let config = default_runtime_config();
        reconcile_review_runtime_for_run(&brehon_root, &[], "supervisor", &config).unwrap();

        let task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(brehon_root.join("runtime").join("tasks").join("T-4.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(task["status"].as_str(), Some("changes_requested"));
        assert_eq!(task["assignee"].as_str(), Some("worker-4"));
    }

    #[test]
    fn test_reconcile_review_runtime_preserves_shared_collecting_rounds_on_restart() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let review_dir = brehon_root.join("runtime").join("reviews").join("T-shared");
        std::fs::create_dir_all(&review_dir).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("review-panels")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("prompt-queue")).unwrap();

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join("T-shared.json"),
            serde_json::json!({
                "task_id": "T-shared",
                "title": "Shared review task",
                "status": "in_review",
                "assignee": serde_json::Value::Null,
                "review_owner": "worker-9",
                "task_type": "task"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            review_dir.join("state.json"),
            serde_json::to_string_pretty(&RuntimeReviewState {
                task_id: "T-shared".to_string(),
                status: "collecting".to_string(),
                current_round: 1,
                cycle_start_round: 1,
                current_review_id: "REV-shared".to_string(),
                max_rounds: 3,
                panel_id: "primary".to_string(),
                panel_mode: "configured_panel".to_string(),
                panel: vec!["reviewer-a".to_string(), "reviewer-b".to_string()],
                submissions_received: vec!["reviewer-a".to_string()],
                created_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("review-panels")
                .join("T-shared.json"),
            serde_json::to_string_pretty(&RuntimePanelLeaseState {
                panel_id: "primary".to_string(),
                task_id: "T-shared".to_string(),
                review_id: "REV-shared".to_string(),
                round: 1,
                members: vec![
                    RuntimePanelLeaseMember {
                        slot_agent: "codex-reviewer".to_string(),
                        reviewer: "reviewer-a".to_string(),
                    },
                    RuntimePanelLeaseMember {
                        slot_agent: "gemini-reviewer".to_string(),
                        reviewer: "reviewer-b".to_string(),
                    },
                ],
                leased_at: "2026-04-09T00:00:00Z".to_string(),
                updated_at: "2026-04-09T00:00:00Z".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let mut config = default_runtime_config();
        config.review.lease_mode = ReviewLeaseMode::ShareAfterSubmit;
        // ShareAfterSubmit preservation only kicks in when no panels are
        // configured (see `reconcile_review_runtime_for_run`). The default
        // config loads `panels: [{id: primary}]`, so without this the test
        // falls through to the "no planned panel" releasing branch and the
        // task status flips to `changes_requested`.
        config.review.panels = Vec::new();
        reconcile_review_runtime_for_run(&brehon_root, &[], "supervisor", &config).unwrap();

        assert!(
            review_dir.join("state.json").exists(),
            "shared collecting rounds should survive restart"
        );
        assert!(
            !brehon_root
                .join("runtime")
                .join("review-panels")
                .join("T-shared.json")
                .exists(),
            "shared collecting rounds should not keep stale lease files"
        );

        let task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-shared.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(task["status"].as_str(), Some("in_review"));
        assert_eq!(task["assignee"].as_str(), Some("worker-9"));

        let queued_files = std::fs::read_dir(brehon_root.join("runtime").join("prompt-queue"))
            .unwrap()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(queued_files.len(), 1);
        let prompt: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(queued_files[0].path()).unwrap())
                .unwrap();
        assert!(prompt["message"]
            .as_str()
            .unwrap()
            .contains("Preserved in-flight shared reviewer round"));
    }
}
