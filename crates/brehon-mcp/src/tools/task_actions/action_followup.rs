//! Handlers for followup task actions: followups, promote_followups, waive_followups.

use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{error_result, text_result};

use super::dependencies::parse_task_id_list_arg;
use super::followups::{
    followup_id, is_open_followup, read_followups_field, write_followups_field,
};
use super::lifecycle::{
    caller_role, reconcile_dependency_states_with_task_lock, task_completion_mode_from_task,
};
use super::locking::acquire_task_lock;
use super::persistence::{read_task, write_task};

pub(super) async fn execute_followups(args: &Value) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };
    let include_resolved = args
        .get("include_resolved_followups")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let Some(task) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };
    let mut followups = read_followups_field(&task);
    if !include_resolved {
        followups.retain(is_open_followup);
    }
    let severity_counts =
        followups
            .iter()
            .fold(BTreeMap::<String, usize>::new(), |mut acc, followup| {
                let key = followup
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                *acc.entry(key).or_insert(0) += 1;
                acc
            });
    let result = serde_json::json!({
        "status": "ok",
        "task_id": id,
        "count": followups.len(),
        "followups": followups,
        "include_resolved": include_resolved,
        "recommended_action": if !include_resolved { "promote_followups" } else { "inspect" },
        "severity_counts": severity_counts,
        "message": "Approved-review followups are durable debt. Default action is promote_followups; waive_followups is for explicit no-action-needed items only."
    });
    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_promote(args: &Value) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };
    let caller_role = caller_role(args);
    if caller_role != "supervisor" {
        return Ok(error_result(
            "Only supervisors can promote followups into a new task.",
        ));
    }
    let requested_followup_ids: HashSet<String> = match parse_task_id_list_arg(args, "followup_ids")
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(err) => return Ok(error_result(err)),
    };
    let followup_title = args
        .get("followup_title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from);
    let _lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };
    let Some(mut source_task) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };
    let mut followups = read_followups_field(&source_task);
    let mut selected = Vec::new();
    for followup in &followups {
        if !is_open_followup(followup) {
            continue;
        }
        let Some(current_id) = followup_id(followup) else {
            continue;
        };
        if requested_followup_ids.is_empty() || requested_followup_ids.contains(current_id) {
            selected.push(followup.clone());
        }
    }
    if selected.is_empty() {
        return Ok(error_result(format!(
            "Task {id} has no matching open followups to promote."
        )));
    }

    let new_task_id = format!(
        "T-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("followup")
    );
    let now = chrono::Utc::now().to_rfc3339();
    let source_title = source_task
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("(untitled)");
    let mut new_task = serde_json::Map::new();
    new_task.insert("task_id".into(), Value::String(new_task_id.clone()));
    new_task.insert(
        "title".into(),
        Value::String(followup_title.unwrap_or_else(|| format!("Follow-ups for {source_title}"))),
    );
    new_task.insert(
        "description".into(),
        Value::String(format!(
            "Follow-up work promoted from task {id}.\n\nOpen review followups:\n{}",
            selected
                .iter()
                .enumerate()
                .map(|(idx, followup)| {
                    let description = followup
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(missing description)");
                    let suggestion = followup
                        .get("suggestion")
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.trim().is_empty())
                        .map(|v| format!(" Suggestion: {v}"))
                        .unwrap_or_default();
                    format!("{}. {}{}", idx + 1, description, suggestion)
                })
                .collect::<Vec<_>>()
                .join("\n")
        )),
    );
    new_task.insert("status".into(), Value::String("pending".to_string()));
    new_task.insert("task_type".into(), Value::String("task".to_string()));
    let source_completion_mode = task_completion_mode_from_task(&source_task);
    new_task.insert(
        "completion_mode".into(),
        Value::String(source_completion_mode.as_str().to_string()),
    );
    new_task.insert("created_at".into(), Value::String(now.clone()));
    new_task.insert("updated_at".into(), Value::String(now.clone()));
    new_task.insert("assignee".into(), Value::Null);
    new_task.insert("percent".into(), Value::Number(0.into()));
    new_task.insert("source_task_id".into(), Value::String(id.to_string()));
    new_task.insert("review_followup_task".into(), Value::Bool(true));
    new_task.insert(
        "plan_steps".into(),
        Value::Array(vec![
            Value::String("Review the promoted followup list".to_string()),
            Value::String("Implement the cleanup or improvement".to_string()),
            Value::String("Re-run relevant verification".to_string()),
        ]),
    );
    new_task.insert(
        "acceptance_criteria".into(),
        Value::Array(
            selected
                .iter()
                .map(|followup| {
                    Value::String(
                        followup
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(missing description)")
                            .to_string(),
                    )
                })
                .collect(),
        ),
    );
    if let Some(parent_id) = source_task
        .get("parent_id")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
    {
        new_task.insert("parent_id".into(), Value::String(parent_id.to_string()));
    }
    if let Some(merge_target) = source_task
        .get("merge_target")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
    {
        new_task.insert(
            "merge_target".into(),
            Value::String(merge_target.to_string()),
        );
    }
    if let Some(integration_status) = source_task
        .get("integration_status")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
    {
        new_task.insert(
            "integration_status".into(),
            Value::String(integration_status.to_string()),
        );
    }
    if source_task
        .get("direct_to_main")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        new_task.insert("direct_to_main".into(), Value::Bool(true));
    }
    let new_task_lock = match acquire_task_lock(&new_task_id).await {
        Ok(lock) => lock,
        Err(err) => {
            return Ok(error_result(format!(
                "Failed to lock promoted followup task {new_task_id}: {err}"
            )));
        }
    };
    if !write_task(&new_task_id, &new_task) {
        return Ok(error_result(format!(
            "Failed to persist promoted followup task {new_task_id}"
        )));
    }
    drop(new_task_lock);

    let promoted_ids: HashSet<&str> = selected.iter().filter_map(followup_id).collect();
    for followup in &mut followups {
        let Some(current_id) = followup_id(followup).map(str::to_string) else {
            continue;
        };
        if !promoted_ids.contains(current_id.as_str()) {
            continue;
        }
        if let Some(object) = followup.as_object_mut() {
            object.insert("status".into(), Value::String("tasked".to_string()));
            object.insert(
                "followup_task_id".into(),
                Value::String(new_task_id.clone()),
            );
            object.insert("updated_at".into(), Value::String(now.clone()));
        }
    }
    write_followups_field(&mut source_task, &followups);
    source_task.insert("updated_at".into(), Value::String(now));
    if !write_task(id, &source_task) {
        return Ok(error_result(format!(
            "Failed to update followup state on task {id}"
        )));
    }
    if let Err(err) = reconcile_dependency_states_with_task_lock(id).await {
        return Ok(error_result(err));
    }

    let result = serde_json::json!({
        "status": "ok",
        "task_id": id,
        "action": "promote_followups",
        "followup_task_id": new_task_id,
        "promoted_followups": selected,
    });
    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_waive(args: &Value) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };
    let caller_role = caller_role(args);
    if caller_role != "supervisor" {
        return Ok(error_result("Only supervisors can waive followups."));
    }
    let reason = match args.get("reason").and_then(|v| v.as_str()) {
        Some(reason) if !reason.trim().is_empty() => reason.trim().to_string(),
        _ => return Ok(error_result("Missing required parameter: reason")),
    };
    let waive_all = args
        .get("waive_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let requested_followup_ids: HashSet<String> = match parse_task_id_list_arg(args, "followup_ids")
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(err) => return Ok(error_result(err)),
    };
    let _lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };
    let Some(mut task) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };
    let mut followups = read_followups_field(&task);
    let open_followups: Vec<Value> = followups
        .iter()
        .filter(|followup| is_open_followup(followup))
        .cloned()
        .collect();
    if requested_followup_ids.is_empty() && open_followups.len() > 1 && !waive_all {
        return Ok(error_result(format!(
            "Task {id} has {} open followups. Do not blanket-waive them by default. \
             Use task action=promote_followups id={id} to create follow-up work, or pass explicit followup_ids. \
             If you truly intend to waive every open followup, rerun with waive_all=true and a specific reason.",
            open_followups.len()
        )));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut waived_ids = Vec::new();
    for followup in &mut followups {
        if !is_open_followup(followup) {
            continue;
        }
        let Some(current_id) = followup_id(followup).map(str::to_string) else {
            continue;
        };
        if !requested_followup_ids.is_empty() && !requested_followup_ids.contains(&current_id) {
            continue;
        }
        if let Some(object) = followup.as_object_mut() {
            object.insert("status".into(), Value::String("waived".to_string()));
            object.insert("waived_reason".into(), Value::String(reason.clone()));
            object.insert("updated_at".into(), Value::String(now.clone()));
            waived_ids.push(current_id);
        }
    }
    if waived_ids.is_empty() {
        return Ok(error_result(format!(
            "Task {id} has no matching open followups to waive."
        )));
    }
    write_followups_field(&mut task, &followups);
    task.insert("updated_at".into(), Value::String(now));
    if !write_task(id, &task) {
        return Ok(error_result(format!(
            "Failed to update followups on task {id}"
        )));
    }
    if let Err(err) = reconcile_dependency_states_with_task_lock(id).await {
        return Ok(error_result(err));
    }
    let result = serde_json::json!({
        "status": "ok",
        "task_id": id,
        "action": "waive_followups",
        "waived_followup_ids": waived_ids,
        "reason": reason,
    });
    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}
