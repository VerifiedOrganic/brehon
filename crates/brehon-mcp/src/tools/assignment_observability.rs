use crate::tools::agent::resolve_delivery_status;
use brehon_types::{pane_assignment_context_path, PaneAssignmentContext};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;

pub(crate) const TASK_ASSIGNMENT_PROPAGATION_FIELD: &str = "assignment_propagation";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AssignmentPropagation {
    pub owner: String,
    pub assignment_kind: String,
    pub assigned_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_started_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_started_via: Option<String>,
}

impl AssignmentPropagation {
    pub(crate) fn new(
        owner: &str,
        assignment_kind: &str,
        prompt_id: Option<String>,
        delivery_method: Option<String>,
    ) -> Self {
        Self {
            owner: owner.to_string(),
            assignment_kind: assignment_kind.to_string(),
            assigned_at: now_rfc3339(),
            prompt_id,
            delivery_method,
            acknowledged_at: None,
            acknowledged_by: None,
            acknowledged_via: None,
            progress_started_at: None,
            progress_started_by: None,
            progress_started_via: None,
        }
    }
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn acknowledge_propagation(
    propagation: &mut AssignmentPropagation,
    actor: &str,
    via: &str,
) -> bool {
    let actor = actor.trim();
    if actor.is_empty() {
        return false;
    }

    let already_matches = propagation.acknowledged_at.is_some()
        && propagation.acknowledged_by.as_deref() == Some(actor)
        && propagation.acknowledged_via.as_deref() == Some(via);
    if already_matches {
        return false;
    }

    propagation.acknowledged_at = Some(now_rfc3339());
    propagation.acknowledged_by = Some(actor.to_string());
    propagation.acknowledged_via = Some(via.to_string());
    true
}

pub(crate) fn mark_progress_started(
    propagation: &mut AssignmentPropagation,
    actor: &str,
    via: &str,
) -> bool {
    let actor = actor.trim();
    if actor.is_empty() {
        return false;
    }

    let already_matches = propagation.progress_started_at.is_some()
        && propagation.progress_started_by.as_deref() == Some(actor)
        && propagation.progress_started_via.as_deref() == Some(via);
    if already_matches {
        return false;
    }

    propagation.progress_started_at = Some(now_rfc3339());
    propagation.progress_started_by = Some(actor.to_string());
    propagation.progress_started_via = Some(via.to_string());
    true
}

pub(crate) fn read_task_assignment_propagation(
    task: &Map<String, Value>,
) -> Option<AssignmentPropagation> {
    task.get(TASK_ASSIGNMENT_PROPAGATION_FIELD)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn write_task_assignment_propagation(
    task: &mut Map<String, Value>,
    propagation: &AssignmentPropagation,
) {
    if let Ok(value) = serde_json::to_value(propagation) {
        task.insert(TASK_ASSIGNMENT_PROPAGATION_FIELD.to_string(), value);
    }
}

fn brehon_root_dir() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT").ok().and_then(|root| {
        let root = root.trim();
        if root.is_empty() {
            None
        } else {
            Some(PathBuf::from(root))
        }
    })
}

pub(crate) fn read_pane_assignment_context(pane_id: &str) -> Option<PaneAssignmentContext> {
    let path = pane_assignment_context_path(&brehon_root_dir()?, pane_id);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn build_assignment_observability(
    owner: &str,
    assignment_kind: &str,
    task_id: &str,
    review_id: Option<&str>,
    round: Option<u32>,
    propagation: Option<&AssignmentPropagation>,
    progress_started: bool,
) -> Value {
    let prompt_id = propagation
        .and_then(|receipt| receipt.prompt_id.as_deref())
        .filter(|value| !value.trim().is_empty());
    let delivery = prompt_id.map(resolve_delivery_status);
    let delivered = delivery
        .as_ref()
        .is_some_and(|status| status.overall == "injected");
    let active_context = read_pane_assignment_context(owner);
    let context_matches = active_context.as_ref().is_some_and(|context| {
        context.assignment_kind == assignment_kind
            && context.task_id == task_id
            && review_id.is_none_or(|expected| context.review_id.as_deref() == Some(expected))
            && round.is_none_or(|expected| context.round == Some(expected))
    });
    let acknowledged = propagation
        .and_then(|receipt| receipt.acknowledged_at.as_deref())
        .is_some();
    let overall = if !delivered {
        "assigned_without_delivery"
    } else if !acknowledged {
        "delivered_without_ack"
    } else if !context_matches {
        "acked_without_context"
    } else if !progress_started {
        "acked_without_progress"
    } else {
        "active"
    };

    let delivery_value = if let Some(status) = delivery {
        serde_json::json!({
            "prompt_id": prompt_id,
            "state": status.overall,
            "enqueued": status.enqueued,
            "enqueued_at": status.enqueued_at,
            "queued": status.queued,
            "injected": status.injected,
            "injected_at": status.injected_at,
            "injected_method": status.injected_method,
            "target": status.target,
            "dead_lettered": status.dead_lettered,
            "dead_letter_reason": status.dead_letter_reason,
            "message": status.human_summary,
        })
    } else {
        serde_json::json!({
            "prompt_id": prompt_id,
            "state": "not_enqueued",
            "enqueued": false,
            "queued": false,
            "injected": false,
            "dead_lettered": false,
            "message": "No assignment prompt_id was recorded for this assignment.",
        })
    };

    let active_context_value = match active_context {
        Some(context) => serde_json::json!({
            "present": true,
            "matches": context_matches,
            "assignment_kind": context.assignment_kind,
            "task_id": context.task_id,
            "review_id": context.review_id,
            "round": context.round,
            "status": context.status,
            "updated_at": context.updated_at,
        }),
        None => serde_json::json!({
            "present": false,
            "matches": false,
        }),
    };

    serde_json::json!({
        "owner": owner,
        "assignment_kind": assignment_kind,
        "assigned_at": propagation.map(|receipt| receipt.assigned_at.clone()),
        "acknowledged_at": propagation.and_then(|receipt| receipt.acknowledged_at.clone()),
        "acknowledged_by": propagation.and_then(|receipt| receipt.acknowledged_by.clone()),
        "acknowledged_via": propagation.and_then(|receipt| receipt.acknowledged_via.clone()),
        "progress_started_at": propagation.and_then(|receipt| receipt.progress_started_at.clone()),
        "progress_started_by": propagation.and_then(|receipt| receipt.progress_started_by.clone()),
        "progress_started_via": propagation.and_then(|receipt| receipt.progress_started_via.clone()),
        "delivery": delivery_value,
        "active_context": active_context_value,
        "progress_started": progress_started,
        "overall": overall,
    })
}
