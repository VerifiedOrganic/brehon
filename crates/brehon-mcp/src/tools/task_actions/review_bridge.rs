//! Review bridge: task-level state transitions driven by the review subsystem.

use std::path::PathBuf;

use serde_json::Value;

use brehon_types::normalize_task_status;

use crate::tools::agent::{current_runtime_session_name_from_root, session_is_live};

use super::lifecycle::reconcile_dependency_states_with_task_lock;
use super::locking::acquire_task_lock;
use super::persistence::{read_task, write_task};

fn runtime_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("BREHON_ROOT")
        .map(PathBuf::from)
        .map(|root| root.join("runtime").join("sessions"))
}

fn review_owner_is_live(owner: &str) -> bool {
    let owner = owner.trim();
    if owner.is_empty() {
        return false;
    }

    let Some(sessions_dir) = runtime_sessions_dir() else {
        return true;
    };
    if !sessions_dir.exists() {
        return true;
    }

    let session_path = sessions_dir.join(format!("{owner}.json"));
    let Ok(content) = std::fs::read_to_string(session_path) else {
        return false;
    };
    let Ok(session) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    if session.get("role").and_then(|value| value.as_str()) != Some("worker") {
        return false;
    }
    if let Some(brehon_root) = std::env::var_os("BREHON_ROOT").map(PathBuf::from) {
        if let Some(expected_session) = current_runtime_session_name_from_root(&brehon_root) {
            let session_name = session
                .get("session_name")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if session_name != Some(expected_session.as_str()) {
                return false;
            }
        }
    }

    session_is_live(&session)
}

pub(crate) async fn set_task_review_feedback(
    task_id: &str,
    feedback: Option<Value>,
) -> Result<(), String> {
    let _lock = acquire_task_lock(task_id).await?;

    let Some(mut task) = read_task(task_id) else {
        return Err(format!("Task not found: {task_id}"));
    };

    match feedback {
        Some(feedback) => {
            task.insert("review_feedback".into(), feedback);
        }
        None => {
            task.remove("review_feedback");
        }
    }

    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );

    if !write_task(task_id, &task) {
        return Err(format!("Failed to write task {task_id}"));
    }
    reconcile_dependency_states_with_task_lock(task_id).await?;

    Ok(())
}

pub(crate) async fn release_task_worker_to_review(
    task_id: &str,
    owner_hint: Option<&str>,
) -> Result<Option<String>, String> {
    let _lock = acquire_task_lock(task_id).await?;

    let Some(mut task) = read_task(task_id) else {
        return Err(format!("Task not found: {task_id}"));
    };

    let owner = task
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| {
            task.get("review_owner")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(String::from)
        })
        .or_else(|| {
            owner_hint
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(String::from)
        });

    if let Some(ref owner) = owner {
        task.insert("assignee".into(), Value::String(owner.clone()));
        task.insert("review_owner".into(), Value::String(owner.clone()));
    }
    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );

    if !write_task(task_id, &task) {
        return Err(format!("Failed to write task {task_id}"));
    }
    reconcile_dependency_states_with_task_lock(task_id).await?;

    Ok(owner)
}

pub(crate) async fn restore_task_worker_from_review_owner(
    task_id: &str,
) -> Result<Option<String>, String> {
    let _lock = acquire_task_lock(task_id).await?;

    let Some(mut task) = read_task(task_id) else {
        return Err(format!("Task not found: {task_id}"));
    };

    let owner = task
        .get("review_owner")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| review_owner_is_live(value))
        .map(String::from);

    task.insert(
        "assignee".into(),
        owner
            .as_ref()
            .map(|value| Value::String(value.clone()))
            .unwrap_or(Value::Null),
    );
    if matches!(
        task.get("status").and_then(|value| value.as_str()),
        Some("changes_requested")
    ) {
        task.insert(
            "percent".into(),
            Value::Number(serde_json::Number::from(0_u64)),
        );
        task.remove("activity");
    }
    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );

    if !write_task(task_id, &task) {
        return Err(format!("Failed to write task {task_id}"));
    }
    reconcile_dependency_states_with_task_lock(task_id).await?;

    Ok(owner)
}

/// Atomically update task status with lock acquisition and updated_at timestamp.
///
/// This is the SINGLE authority for review-driven task status transitions.
/// - Acquires exclusive lock before writing (prevents concurrent writes)
/// - Updates `updated_at` timestamp (audit trail)
/// - Validates the transition for review outcomes (in_review → approved/changes_requested)
/// - Atomic write (temp file + rename)
///
/// Used by verification.rs for review→task state transitions.
/// Returns Ok(()) on success, Err with description on failure.
pub async fn update_task_status_atomic(task_id: &str, new_status: &str) -> Result<(), String> {
    let normalized = normalize_task_status(new_status)
        .ok_or_else(|| format!("Invalid status value: {new_status}"))?;

    let _lock = acquire_task_lock(task_id).await?;

    let Some(mut task) = read_task(task_id) else {
        return Err(format!("Task not found: {task_id}"));
    };

    let current_status = task
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let current_normalized = normalize_task_status(current_status)
        .ok_or_else(|| format!("Unknown current status: {current_status}"))?;

    if current_normalized == normalized {
        return Ok(());
    }

    let valid: &[&str] = match current_normalized {
        "in_review" => &["changes_requested", "approved", "blocked"],
        "review_ready" => &["in_review", "changes_requested"],
        "in_progress" => &["in_review", "review_ready"],
        "changes_requested" => &["in_review", "approved", "review_ready", "blocked"],
        "blocked" => &["review_ready"],
        "approved" => &["changes_requested"],
        _ => &[],
    };

    if !valid.contains(&normalized) {
        let valid_desc = if valid.is_empty() {
            "none (review cannot transition from this state)".to_string()
        } else {
            valid.join(", ")
        };
        return Err(format!(
            "Invalid review outcome transition: '{current_normalized}' → '{normalized}'. \
             Valid transitions from '{current_normalized}': {valid_desc}"
        ));
    }

    task.insert("status".into(), Value::String(normalized.to_string()));
    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );

    if !write_task(task_id, &task) {
        return Err(format!("Failed to write task {task_id}"));
    }

    Ok(())
}
