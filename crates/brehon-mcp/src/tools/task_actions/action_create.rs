//! Handler for the "create" task action.

use serde_json::Value;

use brehon_types::{parse_task_completion_mode, TaskCompletionMode};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{error_result, text_result};

use super::dependencies::{parse_task_id_list_arg, write_string_list_field};
use super::epic::{
    container_base_branch_for_parent, default_container_integration_branch,
    ensure_container_integration_worktree,
};
use super::git_ops::detect_default_branch;
use super::lifecycle::{
    allows_parent, direct_to_main_requested, is_container_task, is_epic, is_initiative,
    parent_can_contain, reconcile_dependency_states_with_task_lock,
    task_completion_mode_from_fields,
};
use super::locking::acquire_task_lock;
use super::persistence::{read_task, write_task};
use super::structured_spec::{
    compose_task_description, control_plane_scope_issue_for_brief, epic_needs_integration_branch,
    read_structured_task_spec, resolve_task_brief, store_structured_task_spec, validate_task_brief,
};

pub(super) async fn execute(args: &Value) -> Result<ToolResult, McpError> {
    let title = match args.get("title").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(error_result("Missing required parameter: title")),
    };

    let short_id = &uuid::Uuid::new_v4().to_string()[..8];
    let task_id = format!("T-{short_id}");
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let task_type = args
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    if !matches!(task_type, "task" | "epic" | "initiative") {
        return Ok(error_result(
            "Invalid task_type. Expected one of: task, epic, initiative",
        ));
    }
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let explicit_structured_spec = match read_structured_task_spec(args) {
        Ok(spec) => spec,
        Err(err) => return Ok(error_result(err)),
    };
    let dependencies = match parse_task_id_list_arg(args, "dependencies") {
        Ok(items) => items,
        Err(err) => return Ok(error_result(err)),
    };
    let (description_summary, structured_spec) =
        resolve_task_brief(description, explicit_structured_spec);
    let explicit_completion_mode = args.get("completion_mode").and_then(|v| v.as_str());
    if explicit_completion_mode.is_some()
        && explicit_completion_mode
            .and_then(parse_task_completion_mode)
            .is_none()
    {
        return Ok(error_result(
            "Invalid completion_mode. Expected one of: merge, close",
        ));
    }
    let completion_mode =
        task_completion_mode_from_fields(task_type, title, description, explicit_completion_mode);
    let direct_to_main = direct_to_main_requested(args);
    if direct_to_main
        && !is_epic(task_type)
        && explicit_completion_mode
            .and_then(parse_task_completion_mode)
            .is_some_and(|mode| mode != TaskCompletionMode::Merge)
    {
        return Ok(error_result(
            "direct_to_main only applies to merge-mode subtasks. Close-mode tasks do not merge to the default branch.",
        ));
    }
    if let Err(err) = validate_task_brief(
        task_type,
        completion_mode,
        &description_summary,
        &structured_spec,
    ) {
        return Ok(error_result(err));
    }
    if task_type == "task" {
        if let Some(scope_issue) =
            control_plane_scope_issue_for_brief(&description_summary, &structured_spec)
        {
            return Ok(error_result(format!(
                "Concrete worker tasks cannot target live Brehon control-plane state. {} \
                 Handle .brehon config/runtime repairs as supervisor-controlled maintenance instead of dispatching them to a worker.",
                scope_issue
            )));
        }
    }
    let rendered_description = compose_task_description(&description_summary, &structured_spec);
    let explicit_integration_branch = args
        .get("integration_branch")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty());
    let has_parent = args
        .get("parent_id")
        .and_then(|v| v.as_str())
        .is_some_and(|v| !v.is_empty());
    let parent_task = if let Some(parent_id) = args
        .get("parent_id")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
    {
        let Some(parent_task) = read_task(parent_id) else {
            return Ok(error_result(format!("Parent task not found: {parent_id}")));
        };
        let parent_type = parent_task
            .get("task_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !parent_can_contain(parent_type, task_type) {
            return Ok(error_result(format!(
                "Invalid hierarchy: parent {parent_id} is type={parent_type}, which cannot contain child type={task_type}."
            )));
        }
        Some(parent_task)
    } else {
        None
    };

    if !allows_parent(task_type) && has_parent {
        return Ok(error_result(
            "Initiatives cannot be nested under another parent task.",
        ));
    }
    if is_epic(task_type) && direct_to_main && explicit_integration_branch.is_some() {
        return Ok(error_result(
            "Epics cannot set both integration_branch and direct_to_main=true. Choose epic-branch integration or explicit direct-to-main flow.",
        ));
    }
    if is_epic(task_type)
        && direct_to_main
        && parent_task
            .as_ref()
            .and_then(|parent| parent.get("task_type").and_then(|v| v.as_str()))
            .is_some_and(is_initiative)
    {
        return Ok(error_result(
            "Epics under an initiative cannot use direct_to_main=true. They must integrate into the initiative branch, and only the initiative may merge to the default branch.",
        ));
    }
    if direct_to_main && !is_epic(task_type) && completion_mode != TaskCompletionMode::Merge {
        return Ok(error_result(
            "direct_to_main only applies to merge-mode subtasks. Close-mode tasks do not merge to the default branch.",
        ));
    }
    if direct_to_main && !is_epic(task_type) && !has_parent {
        return Ok(error_result(
            "direct_to_main is only valid for epics or merge-mode subtasks under an epic. Standalone merge tasks already close directly to the default branch.",
        ));
    }

    let mut task = serde_json::Map::new();
    task.insert("task_id".into(), Value::String(task_id.clone()));
    task.insert("title".into(), Value::String(title.to_string()));
    task.insert("status".into(), Value::String("pending".to_string()));
    task.insert("task_type".into(), Value::String(task_type.to_string()));
    task.insert(
        "completion_mode".into(),
        Value::String(completion_mode.as_str().to_string()),
    );
    task.insert("created_at".into(), Value::String(now));
    task.insert("assignee".into(), Value::Null);
    task.insert("percent".into(), Value::Number(0.into()));
    if is_container_task(task_type) {
        let branch = explicit_integration_branch.map(str::to_string).or_else(|| {
            if is_initiative(task_type)
                || (!direct_to_main
                    && epic_needs_integration_branch(title, &description_summary, &structured_spec))
            {
                Some(default_container_integration_branch(
                    &task_id, title, task_type,
                ))
            } else {
                None
            }
        });
        if let Some(branch) = branch {
            let base_branch =
                match container_base_branch_for_parent(task_type, parent_task.as_ref()) {
                    Ok(branch) => branch,
                    Err(err) => return Ok(error_result(err)),
                };
            let worktree = match ensure_container_integration_worktree(
                &task_id,
                task_type,
                &branch,
                None,
                true,
                false,
                Some(&base_branch),
            )
            .await
            {
                Ok(path) => path,
                Err(err) => return Ok(error_result(err)),
            };
            task.insert("integration_branch".into(), Value::String(branch));
            task.insert(
                "integration_worktree".into(),
                Value::String(worktree.to_string_lossy().to_string()),
            );
        }
        if is_epic(task_type) && direct_to_main {
            task.insert("direct_to_main".into(), Value::Bool(true));
        }
    }

    if !rendered_description.is_empty() {
        task.insert("description".into(), Value::String(rendered_description));
    }
    write_string_list_field(&mut task, "dependencies", &dependencies);
    store_structured_task_spec(&mut task, &structured_spec);
    if let Some(pri) = args.get("priority").and_then(|v| v.as_str()) {
        task.insert("priority".into(), Value::String(pri.to_string()));
    }
    if let Some(policy) = args.get("execution_policy") {
        if !policy.is_object() {
            return Ok(error_result(
                "Invalid execution_policy. Expected an object.",
            ));
        }
        task.insert("execution_policy".into(), policy.clone());
    }

    // Set parent linkage and merge-target fields for worker subtasks.
    if let Some(parent_id) = args.get("parent_id").and_then(|v| v.as_str()) {
        if !parent_id.is_empty() {
            task.insert("parent_id".into(), Value::String(parent_id.to_string()));

            if task_type == "task" {
                let parent_branch = parent_task.as_ref().and_then(|task| {
                    task.get("integration_branch")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                });
                if let Some(parent_branch) = parent_branch {
                    if direct_to_main {
                        return Ok(error_result(format!(
                            "Parent epic {parent_id} already uses integration_branch '{parent_branch}'. \
                             Remove direct_to_main=true and let this subtask inherit the epic branch."
                        )));
                    }
                    task.insert(
                        "merge_target".into(),
                        Value::String(parent_branch.to_string()),
                    );
                    let integration_status = if completion_mode == TaskCompletionMode::Merge {
                        "pending"
                    } else {
                        "not_applicable"
                    };
                    task.insert(
                        "integration_status".into(),
                        Value::String(integration_status.to_string()),
                    );
                } else if completion_mode == TaskCompletionMode::Merge {
                    if !direct_to_main {
                        return Ok(error_result(format!(
                            "Parent epic {parent_id} has no integration_branch. \
                             Merge-mode subtasks must either inherit a feature-epic branch or be created with direct_to_main=true."
                        )));
                    }
                    let merge_target =
                        detect_default_branch().unwrap_or_else(|_| "main".to_string());
                    task.insert("merge_target".into(), Value::String(merge_target));
                    task.insert("direct_to_main".into(), Value::Bool(true));
                }
            }
        }
    }

    let _lock = match acquire_task_lock(&task_id).await {
        Ok(lock) => lock,
        Err(err) => {
            return Ok(error_result(format!(
                "Failed to lock task {task_id}: {err}"
            )))
        }
    };
    if !write_task(&task_id, &task) {
        return Ok(error_result("Failed to persist task"));
    }
    if let Err(err) = reconcile_dependency_states_with_task_lock(&task_id).await {
        return Ok(error_result(err));
    }

    Ok(text_result(
        serde_json::to_string_pretty(&Value::Object(task))
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}
