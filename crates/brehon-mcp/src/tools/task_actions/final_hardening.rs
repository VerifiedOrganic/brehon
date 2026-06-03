//! Idempotent final-hardening epic backfill and seed task creation.

use serde_json::{json, Value};

use crate::error::McpError;
use crate::server::{ContentBlock, ToolResult};
use crate::tools::{error_result, text_result};

use super::dependencies::{read_string_list_field, write_string_list_field};
use super::lifecycle::{caller_role, is_initiative, parent_can_contain};
use super::locking::acquire_task_lock;
use super::persistence::{read_all_tasks, read_task, write_task};

pub const FINAL_HARDENING_EPIC_TITLE: &str = "Final Hardening and Cross-Epic Cleanup";
pub const FINAL_HARDENING_SEED_TASK_COUNT: usize = FINAL_HARDENING_SEED_TASKS.len();

const FINAL_HARDENING_SEED_TASKS: &[FinalHardeningSeedTask] = &[
    FinalHardeningSeedTask {
        source_id: "final-hardening.triage",
        title: "Final hardening triage",
        completion_mode: "close",
        gate: "All deferred cross-epic cleanup candidates are deduplicated, either converted into concrete hardening tasks or explicitly waived with rationale.",
        plan_step: "Audit review notes, integration friction, supervisor observations, and gatekeeper findings for deferred cleanup candidates.",
    },
    FinalHardeningSeedTask {
        source_id: "final-hardening.seams",
        title: "Resolve deferred cross-epic seams",
        completion_mode: "merge",
        gate: "Known cross-epic seams, duplicated glue, naming drift, and transitional compatibility code have been resolved or explicitly documented as intentional.",
        plan_step: "Fix concrete cross-epic inconsistencies identified by the hardening triage and supervisor-added cleanup tasks.",
    },
    FinalHardeningSeedTask {
        source_id: "final-hardening.validation",
        title: "Final validation and operator readiness pass",
        completion_mode: "merge",
        gate: "The initiative branch passes final validation and has current docs, examples, configuration notes, and operator-facing cleanup.",
        plan_step: "Run final validation, update operator-facing documentation/configuration, and remove stale scaffolding before the initiative closes.",
    },
];

struct FinalHardeningSeedTask {
    source_id: &'static str,
    title: &'static str,
    completion_mode: &'static str,
    gate: &'static str,
    plan_step: &'static str,
}

pub(super) async fn execute_ensure(args: &Value) -> Result<ToolResult, McpError> {
    if caller_role(args) != "supervisor" {
        return Ok(error_result(
            "Only supervisors can ensure final hardening epics.",
        ));
    }

    let initiative_id = match args.get("id").and_then(|value| value.as_str()) {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ => return Ok(error_result("Missing required parameter: id")),
    };

    let Some(initiative) = read_task(initiative_id) else {
        return Ok(error_result(format!("Task not found: {initiative_id}")));
    };
    let task_type = initiative
        .get("task_type")
        .and_then(|value| value.as_str())
        .unwrap_or("task");
    if !is_initiative(task_type) {
        return Ok(error_result(format!(
            "Task {initiative_id} is type={task_type}; ensure_final_hardening requires an initiative."
        )));
    }
    if !parent_can_contain(task_type, "epic") {
        return Ok(error_result(format!(
            "Task {initiative_id} cannot contain final hardening epics."
        )));
    }

    let source_file = args.get("source_file").and_then(|value| value.as_str());
    let phase_epic_ids = phase_epic_ids_for_initiative(initiative_id);
    let (epic_id, epic_created, duplicate_epics) =
        ensure_final_hardening_epic(initiative_id, &phase_epic_ids, source_file).await?;
    let seed_results = ensure_seed_tasks(&epic_id, &phase_epic_ids, source_file).await?;

    let created_seed_count = seed_results
        .iter()
        .filter(|result| result.get("created").and_then(|value| value.as_bool()) == Some(true))
        .count();
    let result = json!({
        "status": "ok",
        "initiative_id": initiative_id,
        "epic": {
            "task_id": epic_id,
            "title": FINAL_HARDENING_EPIC_TITLE,
            "created": epic_created,
            "dependency_count": phase_epic_ids.len(),
        },
        "seed_tasks": seed_results,
        "duplicates": duplicate_epics,
        "execution_policy": default_execution_policy(),
        "message": format!(
            "Final hardening epic {} for initiative {} (created={}, seeded_new_tasks={}).",
            epic_id, initiative_id, epic_created, created_seed_count
        )
    });

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|err| McpError::Serialization(err.to_string()))?,
    ))
}

async fn ensure_final_hardening_epic(
    initiative_id: &str,
    phase_epic_ids: &[String],
    source_file: Option<&str>,
) -> Result<(String, bool, Vec<String>), McpError> {
    let matches = final_hardening_epics_for_initiative(initiative_id);
    let duplicate_epics = matches.iter().skip(1).cloned().collect::<Vec<_>>();
    if let Some(existing_id) = matches.first() {
        let _updated = ensure_task_dependencies(existing_id, phase_epic_ids).await?;
        patch_final_hardening_metadata(
            existing_id,
            "final_hardening_epic",
            None,
            None,
            source_file,
        )
        .await?;
        return Ok((existing_id.clone(), false, duplicate_epics));
    }

    let created = create_task(json!({
        "action": "create",
        "task_type": "epic",
        "parent_id": initiative_id,
        "title": FINAL_HARDENING_EPIC_TITLE,
        "description": final_hardening_epic_summary(initiative_id),
        "dependencies": phase_epic_ids,
        "acceptance_criteria": [
            "All seeded and supervisor-added final hardening tasks are terminal.",
            "Any gatekeeper findings are represented as tasks in this epic or explicitly waived with rationale.",
            "The final initiative branch is validated as a coherent whole after all phase epics land."
        ],
        "plan_steps": [
            "Keep adding concrete cleanup tasks here when the supervisor sees deferred cross-epic issues during the run.",
            "Deduplicate vague cleanup candidates before dispatch and preserve evidence in each task.",
            "Run the seeded hardening tasks only after all imported phase epics are complete."
        ],
        "implementation_notes": "This tail epic is Brehon-owned, not source-plan work. It is the only cleanup debt surface: supervisor observations and gatekeeper findings should feed this epic instead of creating a parallel cleanup lifecycle.",
        "integration_branch": final_hardening_branch(initiative_id),
        "execution_policy": default_execution_policy(),
        "role": "supervisor",
        "agent_name": "final-hardening-ensure"
    }))
    .await?;
    let epic_id = created
        .get("task_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            McpError::Internal("Task tool did not return final hardening epic task_id".into())
        })?
        .to_string();
    patch_final_hardening_metadata(&epic_id, "final_hardening_epic", None, None, source_file)
        .await?;
    Ok((epic_id, true, duplicate_epics))
}

async fn ensure_seed_tasks(
    epic_id: &str,
    phase_epic_ids: &[String],
    source_file: Option<&str>,
) -> Result<Vec<Value>, McpError> {
    let mut results = Vec::with_capacity(FINAL_HARDENING_SEED_TASKS.len());
    let mut prior_seed_id: Option<String> = None;
    for (index, seed) in FINAL_HARDENING_SEED_TASKS.iter().enumerate() {
        let mut dependencies = phase_epic_ids.to_vec();
        if let Some(prior_id) = &prior_seed_id {
            dependencies.push(prior_id.clone());
        }

        let existing = find_seed_task(epic_id, seed);
        let (task_id, created) = if let Some(existing_id) = existing {
            let _updated = ensure_task_dependencies(&existing_id, &dependencies).await?;
            (existing_id, false)
        } else {
            let created = create_task(json!({
                "action": "create",
                "task_type": "task",
                "parent_id": epic_id,
                "completion_mode": seed.completion_mode,
                "dependencies": dependencies,
                "title": seed.title,
                "description": "Seeded final hardening task. This is Brehon-owned cleanup work, not an imported source-plan task.",
                "acceptance_criteria": [seed.gate],
                "file_hints": [
                    "Inspect the initiative integration branch and all phase epic summaries.",
                    "Use supervisor-added hardening tasks and gatekeeper findings as the evidence source."
                ],
                "constraints": [
                    "Do not turn vague cleanup into broad rewrites; split concrete issues into separate hardening tasks when needed.",
                    "Preserve evidence for each cleanup decision: source task, source epic, review finding, integration conflict, operator request, or gatekeeper finding.",
                    "If a candidate is not required before main, explicitly mark it waived or deferred rather than silently expanding scope."
                ],
                "test_requirements": [seed.gate],
                "plan_steps": [
                    seed.plan_step,
                    "Report any new cleanup candidates to the supervisor so they can be added to this final hardening epic as concrete tasks."
                ],
                "implementation_notes": "Seeded by ensure_final_hardening. The supervisor may add more sibling tasks to this epic throughout the run as concrete cross-epic cleanup evidence appears.",
                "execution_policy": default_execution_policy(),
                "role": "supervisor",
                "agent_name": "final-hardening-ensure"
            }))
            .await?;
            let task_id = created
                .get("task_id")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    McpError::Internal(
                        "Task tool did not return final hardening seed task_id".into(),
                    )
                })?
                .to_string();
            (task_id, true)
        };

        patch_final_hardening_metadata(
            &task_id,
            "final_hardening_task",
            Some(seed.source_id),
            Some(index + 1),
            source_file,
        )
        .await?;
        results.push(json!({
            "task_id": task_id,
            "title": seed.title,
            "source_task_id": seed.source_id,
            "created": created,
            "sequence": index + 1,
        }));
        prior_seed_id = Some(task_id);
    }
    Ok(results)
}

async fn create_task(args: Value) -> Result<Value, McpError> {
    let result = super::action_create::execute(&args).await?;
    let text = extract_text(&result);
    if result.is_error.unwrap_or(false) {
        return Err(McpError::Internal(text));
    }
    serde_json::from_str(&text).map_err(|err| McpError::Serialization(err.to_string()))
}

fn extract_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn ensure_task_dependencies(task_id: &str, required: &[String]) -> Result<bool, McpError> {
    let _lock = acquire_task_lock(task_id)
        .await
        .map_err(|err| McpError::Internal(format!("Failed to lock task {task_id}: {err}")))?;
    let Some(mut task) = read_task(task_id) else {
        return Err(McpError::Internal(format!("Task not found: {task_id}")));
    };
    let mut dependencies = read_string_list_field(&task, "dependencies");
    let original = dependencies.clone();
    for required_id in required {
        if !dependencies.iter().any(|existing| existing == required_id) {
            dependencies.push(required_id.clone());
        }
    }
    if dependencies == original {
        return Ok(false);
    }
    write_string_list_field(&mut task, "dependencies", &dependencies);
    if !write_task(task_id, &task) {
        return Err(McpError::Internal(format!(
            "Failed to persist task update for {task_id}"
        )));
    }
    super::lifecycle::reconcile_dependency_states_with_task_lock(task_id)
        .await
        .map_err(McpError::Internal)?;
    Ok(true)
}

async fn patch_final_hardening_metadata(
    task_id: &str,
    kind: &str,
    source_task_id: Option<&str>,
    sequence: Option<usize>,
    source_file: Option<&str>,
) -> Result<(), McpError> {
    let _lock = acquire_task_lock(task_id)
        .await
        .map_err(|err| McpError::Internal(format!("Failed to lock task {task_id}: {err}")))?;
    let Some(mut task) = read_task(task_id) else {
        return Err(McpError::Internal(format!("Task not found: {task_id}")));
    };

    let mut plan_import = task
        .get("plan_import")
        .and_then(|value| value.as_object())
        .cloned()
        .unwrap_or_default();
    plan_import.insert("kind".to_string(), Value::String(kind.to_string()));
    plan_import.insert(
        "source_epic_title".to_string(),
        Value::String(FINAL_HARDENING_EPIC_TITLE.to_string()),
    );
    plan_import.insert(
        "source_epic_id".to_string(),
        Value::String("final-hardening".to_string()),
    );
    if let Some(source_file) = source_file.filter(|value| !value.trim().is_empty()) {
        plan_import.insert(
            "source_file".to_string(),
            Value::String(source_file.to_string()),
        );
    }
    if let Some(source_task_id) = source_task_id {
        plan_import.insert(
            "source_task_id".to_string(),
            Value::String(source_task_id.to_string()),
        );
        plan_import.insert("seeded".to_string(), Value::Bool(true));
    } else {
        plan_import.insert(
            "source_title".to_string(),
            Value::String(FINAL_HARDENING_EPIC_TITLE.to_string()),
        );
    }
    if let Some(sequence) = sequence {
        plan_import.insert("sequence".to_string(), json!(sequence));
    }
    task.insert("plan_import".to_string(), Value::Object(plan_import));
    ensure_execution_policy(&mut task);

    if !write_task(task_id, &task) {
        return Err(McpError::Internal(format!(
            "Failed to persist task metadata for {task_id}"
        )));
    }
    Ok(())
}

fn ensure_execution_policy(task: &mut serde_json::Map<String, Value>) {
    let defaults = default_execution_policy();
    let default_map = defaults.as_object().expect("policy is an object");
    match task.get_mut("execution_policy") {
        Some(Value::Object(existing)) => {
            for (key, value) in default_map {
                existing.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        _ => {
            task.insert("execution_policy".to_string(), defaults);
        }
    }
}

fn phase_epic_ids_for_initiative(initiative_id: &str) -> Vec<String> {
    let mut ids = read_all_tasks()
        .into_iter()
        .filter(|task| {
            task.get("parent_id").and_then(|value| value.as_str()) == Some(initiative_id)
        })
        .filter(|task| task.get("task_type").and_then(|value| value.as_str()) == Some("epic"))
        .filter(|task| !is_final_hardening_epic(task))
        .filter_map(|task| {
            task.get("task_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

fn final_hardening_epics_for_initiative(initiative_id: &str) -> Vec<String> {
    let mut ids = read_all_tasks()
        .into_iter()
        .filter(|task| {
            task.get("parent_id").and_then(|value| value.as_str()) == Some(initiative_id)
        })
        .filter(|task| task.get("task_type").and_then(|value| value.as_str()) == Some("epic"))
        .filter(is_final_hardening_epic)
        .filter_map(|task| {
            task.get("task_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

fn is_final_hardening_epic(task: &serde_json::Map<String, Value>) -> bool {
    task.get("title").and_then(|value| value.as_str()) == Some(FINAL_HARDENING_EPIC_TITLE)
        || task
            .get("plan_import")
            .and_then(|value| value.get("kind"))
            .and_then(|value| value.as_str())
            == Some("final_hardening_epic")
}

fn find_seed_task(epic_id: &str, seed: &FinalHardeningSeedTask) -> Option<String> {
    read_all_tasks()
        .into_iter()
        .filter(|task| task.get("parent_id").and_then(|value| value.as_str()) == Some(epic_id))
        .find(|task| {
            task.get("plan_import")
                .and_then(|value| value.get("source_task_id"))
                .and_then(|value| value.as_str())
                == Some(seed.source_id)
                || task.get("title").and_then(|value| value.as_str()) == Some(seed.title)
        })
        .and_then(|task| {
            task.get("task_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn final_hardening_epic_summary(initiative_id: &str) -> String {
    format!(
        "Tail epic for initiative {initiative_id}. The supervisor keeps this epic as the single work surface for deferred cleanup, cross-epic seams, final validation gaps, and gatekeeper findings. It runs only after the normal initiative epics are complete."
    )
}

fn final_hardening_branch(initiative_id: &str) -> String {
    let initiative_suffix = initiative_id
        .strip_prefix("T-")
        .unwrap_or(initiative_id)
        .to_ascii_lowercase();
    format!("epic/final-hardening-{initiative_suffix}")
}

fn default_execution_policy() -> Value {
    json!({
        "work_class": "final_hardening",
        "preferred_lane": "codex-hardening",
        "preferred_agent_type": "codex",
        "preferred_model": "gpt-5.5",
        "preferred_reasoning_effort": "xhigh",
        "strength": "strong",
        "strict": true,
        "reason": "Final hardening work is cross-epic, high-blast-radius cleanup and validation."
    })
}
