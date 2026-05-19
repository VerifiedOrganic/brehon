//! Argument parsing and dependency field helpers.

use serde_json::Value;

pub(super) fn normalize_list_item(item: &str) -> Option<String> {
    let trimmed = item
        .trim()
        .trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(super) fn parse_string_list_arg(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };

    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .ok_or_else(|| format!("Invalid {key}. Expected an array of strings."))
                    .and_then(|s| {
                        normalize_list_item(s).ok_or_else(|| {
                            format!("Invalid {key}. Items must be non-empty strings.")
                        })
                    })
            })
            .collect(),
        Value::String(text) => Ok(text.lines().filter_map(normalize_list_item).collect()),
        _ => Err(format!(
            "Invalid {key}. Expected a string or array of strings."
        )),
    }
}

pub(super) fn parse_task_id_list_arg(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let items = parse_string_list_arg(args, key)?;
    if items.iter().any(|item| item.trim().is_empty()) {
        return Err(format!(
            "Invalid {key}. Task IDs must be non-empty strings."
        ));
    }
    Ok(items)
}

pub(super) fn read_string_list_field(
    task: &serde_json::Map<String, Value>,
    key: &str,
) -> Vec<String> {
    task.get(key)
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::trim))
        .filter(|value| !value.is_empty())
        .map(String::from)
        .collect()
}

pub(super) fn read_dependency_ids(task: &serde_json::Map<String, Value>) -> Vec<String> {
    read_string_list_field(task, "dependencies")
}

pub(super) fn write_string_list_field(
    task: &mut serde_json::Map<String, Value>,
    key: &str,
    values: &[String],
) {
    if values.is_empty() {
        task.remove(key);
    } else {
        task.insert(
            key.to_string(),
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
    }
}

pub(super) fn task_has_manual_blockers(task: &serde_json::Map<String, Value>) -> bool {
    let Some(blockers) = task.get("blockers").and_then(|value| value.as_str()) else {
        return false;
    };
    let blockers = blockers.trim();
    if blockers.is_empty() {
        return false;
    }

    !task_has_dependency_scoped_blocker_text(task)
        && !task_has_recoverable_worker_state_blocker_text(task)
}

pub(super) fn task_has_dependency_scoped_blocker_text(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let Some(blockers) = task.get("blockers").and_then(|value| value.as_str()) else {
        return false;
    };
    let blockers = blockers.trim();
    if blockers.is_empty() {
        return false;
    }

    let dependencies = read_dependency_ids(task);
    if dependencies.is_empty() {
        return false;
    }

    let blockers_lower = blockers.to_ascii_lowercase();
    dependencies
        .iter()
        .any(|dependency_id| blockers.contains(dependency_id))
        || blockers_lower.contains("dependency dag")
        || blockers_lower.contains("dependency")
        || blockers_lower.contains("dependencies complete")
        || blockers_lower.contains("once unblocked")
        || blockers_lower.contains("still inprogress")
        || blockers_lower.contains("still in progress")
        || blockers_lower.contains("until those dependencies complete")
}

pub(super) fn task_has_recoverable_worker_state_blocker_text(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let Some(blockers) = task.get("blockers").and_then(|value| value.as_str()) else {
        return false;
    };
    let blockers = blockers.trim();
    if blockers.is_empty() {
        return false;
    }

    let blockers_lower = blockers.to_ascii_lowercase();
    blockers_lower.contains("state deadlock")
        || blockers_lower.contains("assignment mismatch")
        || blockers_lower.contains("assignee mismatch")
        || blockers_lower.contains("complete call reports task assigned")
        || blockers_lower.contains("reports task assigned to")
        || blockers_lower.contains("assigned to '' not")
        || blockers_lower.contains("assigned to '' instead")
        || blockers_lower.contains("empty string instead")
        || blockers_lower.contains("cannot checkpoint/complete")
        || blockers_lower.contains("checkpoint created during pending state")
        || blockers_lower.contains("could not move it to review_ready")
        || blockers_lower.contains("requires supervisor reassignment")
        || blockers_lower.contains("supervisor-side reassignment")
        || blockers_lower.contains("need reassignment to complete")
        || blockers_lower.contains("not permitted to complete")
        || blockers_lower.contains("ownership drift")
        || blockers_lower.contains("ownership fix")
        || blockers_lower.contains("preventing worker completion handoff")
        || blockers_lower.contains("rejects completion from non-assignee")
        || blockers_lower.contains("worker progress updates are rejected")
        || (blockers_lower.contains("invalid status transition")
            && blockers_lower.contains("'pending'")
            && blockers_lower.contains("'in_progress'"))
        || (blockers_lower.contains("invalid transition")
            && blockers_lower.contains("pending")
            && blockers_lower.contains("in_progress"))
}

pub(super) fn parse_optional_text_arg(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Some(_) => Err(format!("Invalid {key}. Expected a string.")),
    }
}
