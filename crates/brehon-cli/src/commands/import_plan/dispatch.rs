use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use brehon_mcp::server::ContentBlock;
use brehon_mcp::tools::task_actions::{
    TaskActionsTool, FINAL_HARDENING_EPIC_TITLE, FINAL_HARDENING_SEED_TASK_COUNT,
};
use brehon_mcp::tools::Tool;
use serde_json::{json, Value};

use super::extraction::*;
use super::parsing::*;
use super::types::*;
use super::ExtractMode;

fn phase_task_count(phase: &PlanPhase) -> usize {
    phase
        .epics
        .iter()
        .map(|epic| epic.tasks.len())
        .sum::<usize>()
        + usize::from(phase.gate_task.is_some())
}

fn initiative_summary(plan: &PlanDocument) -> String {
    let mut parts = vec![format!(
        "Imported master plan from {}.",
        plan.path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("plan file")
    )];
    if let Some(project) = &plan.project {
        parts.push(format!("Project: {project}."));
    }
    if let Some(stack) = &plan.stack {
        parts.push(format!("Stack: {stack}."));
    }
    if let Some(target) = &plan.target {
        parts.push(format!("Target: {target}."));
    }
    parts.join(" ")
}

fn phase_epic_summary(phase: &PlanPhase) -> String {
    format!(
        "Imported Phase {} from the source implementation plan. This phase contains {} source epics and {} concrete work items.",
        phase.id,
        phase.epics.len(),
        phase_task_count(phase)
    )
}

fn task_summary(
    phase: &PlanPhase,
    source_epic_id: Option<&str>,
    _source_epic_title: Option<&str>,
    task: &PlanTask,
    is_phase_gate: bool,
) -> String {
    if is_phase_gate {
        format!(
            "Imported phase gate {} for Phase {}. This task represents the source plan's phase-wide completion gate.",
            task.source_id, phase.id
        )
    } else {
        format!(
            "Imported source task {} from Phase {} / Epic {}.",
            task.source_id,
            phase.id,
            source_epic_id.unwrap_or("?")
        )
    }
}

fn task_file_hints(task: &PlanTask, source_hint: String, fallback_hint: String) -> Vec<String> {
    let mut hints = vec![source_hint];
    if let Some(details_doc) = task.details_doc.as_deref() {
        hints.push(format!("Task details packet: {details_doc}"));
    }
    hints.extend(
        task.required_reading
            .iter()
            .map(|path| format!("Required reading: {path}")),
    );
    hints.extend(
        task.context_refs
            .iter()
            .map(|path| format!("Context reference: {path}")),
    );
    hints.push(fallback_hint);
    hints
}

fn task_context_notes(task: &PlanTask) -> String {
    let mut notes = Vec::new();
    if let Some(details_doc) = task.details_doc.as_deref() {
        notes.push(format!("Task details packet: {details_doc}."));
    }
    if !task.required_reading.is_empty() {
        notes.push(format!(
            "Required reading: {}.",
            task.required_reading.join(", ")
        ));
    }
    if !task.context_refs.is_empty() {
        notes.push(format!("Context refs: {}.", task.context_refs.join(", ")));
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!(" {}", notes.join(" "))
    }
}

fn task_file_path(project_root: &Path, task_id: &str) -> PathBuf {
    project_root
        .join(".brehon")
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"))
}

fn patch_task_file(project_root: &Path, task_id: &str, patch: Value) -> Result<()> {
    let path = task_file_path(project_root, task_id);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read task file '{}'", path.display()))?;
    let mut json: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse task JSON '{}'", path.display()))?;
    let target = json
        .as_object_mut()
        .ok_or_else(|| anyhow!("Task JSON is not an object for '{}'", path.display()))?;
    let patch_object = patch
        .as_object()
        .ok_or_else(|| anyhow!("Patch JSON must be an object"))?;
    for (key, value) in patch_object {
        target.insert(key.clone(), value.clone());
    }
    fs::write(&path, serde_json::to_string_pretty(&json)?)
        .with_context(|| format!("Failed to write task file '{}'", path.display()))?;
    Ok(())
}

fn seed_imported_terminal_task_state(project_root: &Path, task_id: &str) -> Result<()> {
    let path = task_file_path(project_root, task_id);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read task file '{}'", path.display()))?;
    let mut json: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse task JSON '{}'", path.display()))?;
    let task = json
        .as_object_mut()
        .ok_or_else(|| anyhow!("Task JSON is not an object for '{}'", path.display()))?;

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    task.insert("status".into(), Value::String("closed".to_string()));
    task.insert(
        "terminal_status".into(),
        Value::String("closed".to_string()),
    );
    task.insert("percent".into(), Value::Number(100.into()));
    task.insert(
        "closed_by".into(),
        Value::String("plan-importer".to_string()),
    );
    task.insert(
        "closed_role".into(),
        Value::String("supervisor".to_string()),
    );
    task.insert("closed_at".into(), Value::String(now.clone()));
    task.insert("updated_at".into(), Value::String(now));

    if task
        .get("parent_id")
        .and_then(|value| value.as_str())
        .is_some()
    {
        let completion_mode = task
            .get("completion_mode")
            .and_then(|value| value.as_str())
            .unwrap_or("merge");
        let integration_status = if completion_mode == "merge" {
            "integrated"
        } else {
            "not_applicable"
        };
        task.insert(
            "integration_status".into(),
            Value::String(integration_status.to_string()),
        );

        if integration_status == "integrated" {
            if let Some(merge_target) = task.get("merge_target").and_then(|value| value.as_str()) {
                task.insert(
                    "merged_branch".into(),
                    Value::String(merge_target.to_string()),
                );
            }
        }
    }

    fs::write(&path, serde_json::to_string_pretty(&json)?)
        .with_context(|| format!("Failed to write task file '{}'", path.display()))?;
    Ok(())
}

fn extract_text(result: &brehon_mcp::server::ToolResult) -> String {
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

async fn call_task_tool(tool: &TaskActionsTool, args: Value) -> Result<Value> {
    let result = tool
        .execute(args.clone())
        .await
        .map_err(|err| anyhow!("Task tool execution failed: {err}"))?;
    let text = extract_text(&result);
    if result.is_error.unwrap_or(false) {
        bail!("{text}");
    }
    serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse task tool JSON response: {text}"))
}

fn ensure_git_repo(project_root: &Path) -> Result<()> {
    git2::Repository::open(project_root)
        .with_context(|| format!("'{}' is not a git repository", project_root.display()))?;
    Ok(())
}

pub async fn execute(
    project_root: &Path,
    plan_path: &Path,
    dry_run: bool,
    mode: ExtractMode,
) -> Result<()> {
    ensure_git_repo(project_root)?;
    let plan = load_plan_document(project_root, plan_path, mode).await?;

    if dry_run {
        println!("Plan: {}", plan.title);
        println!("Source: {}", plan.path.display());
        println!("Phases: {}", plan.phases.len());
        println!(
            "Brehon mapping: 1 initiative, {} phase epics, 1 final hardening epic, {} imported tasks, {} hardening seed tasks",
            plan.phases.len(),
            plan.phases.iter().map(phase_task_count).sum::<usize>(),
            FINAL_HARDENING_SEED_TASK_COUNT
        );
        for phase in &plan.phases {
            println!(
                "- Phase {}: {} | source epics={} tasks={} epic_branch=phase-scoped",
                phase.id,
                phase.title,
                phase.epics.len(),
                phase_task_count(phase)
            );
        }
        return Ok(());
    }

    let brehon_root = project_root.join(".brehon");
    fs::create_dir_all(brehon_root.join("runtime").join("tasks"))?;
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.display().to_string()),
        ("BREHON_PROJECT_ROOT", project_root.display().to_string()),
    ]);
    let tool = TaskActionsTool::new();

    let initiative = call_task_tool(
        &tool,
        json!({
            "action": "create",
            "task_type": "initiative",
            "title": plan.title,
            "description": initiative_summary(&plan),
            "acceptance_criteria": plan.phases.iter().map(|phase| format!("Phase {} is closed after all imported source tasks and its phase gate are complete.", phase.id))
                .chain(std::iter::once("Final hardening epic is closed after deferred cleanup, cross-epic seams, validation gaps, and gatekeeper findings are resolved or explicitly waived.".to_string()))
                .collect::<Vec<_>>(),
            "plan_steps": plan.phases.iter().map(|phase| format!("Phase {}: {}", phase.id, phase.title))
                .chain(std::iter::once(format!("{FINAL_HARDENING_EPIC_TITLE}: seal final cross-epic gaps before initiative close.")))
                .collect::<Vec<_>>(),
            "implementation_notes": format!("Imported from {}. This initiative was generated by brehon import-plan and preserves the source phase/task DAG.", plan.path.display()),
            "role": "supervisor",
            "agent_name": "plan-importer"
        }),
    )
    .await?;
    let initiative_id = initiative["task_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Task tool did not return initiative task_id"))?
        .to_string();

    patch_task_file(
        project_root,
        &initiative_id,
        json!({
            "plan_import": {
                "source_file": plan.path.display().to_string(),
                "kind": "initiative",
                "source_title": plan.title,
                "project": plan.project,
                "stack": plan.stack,
                "target": plan.target
            }
        }),
    )?;

    let mut imported_tasks = Vec::new();

    for phase in &plan.phases {
        let mut phase_acceptance = vec![format!(
            "All imported Phase {} work items are terminal and integrated into the phase branch.",
            phase.id
        )];
        if let Some(gate_task) = &phase.gate_task {
            phase_acceptance.push(format!(
                "Source phase gate {} passes: {}",
                gate_task.source_id, gate_task.gate
            ));
        }
        let mut phase_steps = phase
            .epics
            .iter()
            .map(|epic| {
                format!(
                    "Import and execute source epic {}: {}",
                    epic.source_id, epic.title
                )
            })
            .collect::<Vec<_>>();
        if let Some(gate_task) = &phase.gate_task {
            phase_steps.push(format!(
                "Finish source phase gate {}: {}",
                gate_task.source_id, gate_task.title
            ));
        }

        let epic = call_task_tool(
            &tool,
            json!({
                "action": "create",
                "task_type": "epic",
                "parent_id": initiative_id.clone(),
                "title": format!("Phase {}: {}", phase.id, phase.title),
                "description": phase_epic_summary(phase),
                "acceptance_criteria": phase_acceptance,
                "plan_steps": phase_steps,
                "implementation_notes": if phase.notes.is_empty() {
                    format!("Imported from {}. Source phase contains {} document epics.", plan.path.display(), phase.epics.len())
                } else {
                    format!("{}\n\nImported from {}.", phase.notes.join("\n"), plan.path.display())
                },
                "integration_branch": phase_integration_branch(phase, &initiative_id),
                "role": "supervisor",
                "agent_name": "plan-importer"
            }),
        )
        .await?;
        let epic_id = epic["task_id"]
            .as_str()
            .ok_or_else(|| anyhow!("Task tool did not return phase epic task_id"))?
            .to_string();
        patch_task_file(
            project_root,
            &epic_id,
            json!({
                "plan_import": {
                    "source_file": plan.path.display().to_string(),
                    "kind": "phase_epic",
                    "phase_id": phase.id,
                    "phase_title": phase.title,
                    "source_epics": phase.epics.iter().map(|epic| json!({"id": epic.source_id, "title": epic.title})).collect::<Vec<_>>()
                }
            }),
        )?;

        for source_epic in &phase.epics {
            for task in &source_epic.tasks {
                let file_hints = task_file_hints(
                    task,
                    format!(
                        "Source plan section: Phase {} / Epic {}",
                        phase.id, source_epic.source_id
                    ),
                    "Search this repository for the relevant implementation area.".to_string(),
                );
                let implementation_notes = format!(
                    "Imported source status: {}. Original source epic: {}. If the source plan references absolute paths outside this repository, ignore them and work only from repo-local files in the current worktree.{}",
                    task.source_status,
                    source_epic.source_id,
                    task_context_notes(task)
                );
                let created = call_task_tool(
                    &tool,
                    json!({
                        "action": "create",
                        "task_type": "task",
                        "parent_id": epic_id,
                        "title": task.title,
                        "description": task_summary(phase, Some(&source_epic.source_id), Some(&source_epic.title), task, false),
                        "acceptance_criteria": [task.gate.clone()],
                        "file_hints": file_hints,
                        "constraints": [
                            format!("Imported source task ID: {}", task.source_id),
                            format!("Imported size estimate: {}", task.size),
                            "Respect the imported dependency DAG before starting work.".to_string(),
                            "Do not follow absolute filesystem paths from the source plan outside the current repository/worktree; treat them as documentary context only.".to_string()
                        ],
                        "test_requirements": [format!("Satisfy the source gate exactly: {}", task.gate)],
                        "plan_steps": [
                            "Review imported dependencies and the current repository state before editing.".to_string(),
                            format!("Implement only the scope of source task {}.", task.source_id),
                            "Run the gate tests before requesting review.".to_string()
                        ],
                        "implementation_notes": implementation_notes,
                        "role": "supervisor",
                        "agent_name": "plan-importer"
                    }),
                )
                .await?;
                imported_tasks.push(ImportedTaskRecord {
                    brehon_task_id: created["task_id"]
                        .as_str()
                        .ok_or_else(|| {
                            anyhow!("Task tool did not return task_id for imported task")
                        })?
                        .to_string(),
                    phase_id: phase.id.clone(),
                    phase_title: phase.title.clone(),
                    source_epic_id: Some(source_epic.source_id.clone()),
                    source_epic_title: Some(source_epic.title.clone()),
                    task: task.clone(),
                    is_phase_gate: false,
                });
            }
        }

        if let Some(gate_task) = &phase.gate_task {
            let file_hints = task_file_hints(
                gate_task,
                format!("Source plan section: Phase {} gate", phase.id),
                "Use the already-imported phase tasks and current repository state to verify the integration gate.".to_string(),
            );
            let implementation_notes = format!(
                "Imported source status: {}. This is the phase gate task. If the source plan references absolute paths outside this repository, ignore them and work only from repo-local files in the current worktree.{}",
                gate_task.source_status,
                task_context_notes(gate_task)
            );
            let created = call_task_tool(
                &tool,
                json!({
                    "action": "create",
                    "task_type": "task",
                    "parent_id": epic_id,
                    "completion_mode": "merge",
                    "title": gate_task.title,
                    "description": task_summary(phase, None, None, gate_task, true),
                    "acceptance_criteria": [gate_task.gate.clone()],
                    "file_hints": file_hints,
                    "constraints": [
                        format!("Imported source task ID: {}", gate_task.source_id),
                        format!("Imported size estimate: {}", gate_task.size),
                        "Treat this as the phase-level completion gate.".to_string(),
                        "Do not follow absolute filesystem paths from the source plan outside the current repository/worktree; treat them as documentary context only.".to_string()
                    ],
                    "test_requirements": [format!("Satisfy the source phase gate exactly: {}", gate_task.gate)],
                    "plan_steps": [
                        "Wait until the source phase dependencies are complete.".to_string(),
                        format!("Run the integration validation for phase {}.", phase.id),
                        "Only request review after the full gate passes.".to_string()
                    ],
                    "implementation_notes": implementation_notes,
                    "role": "supervisor",
                    "agent_name": "plan-importer"
                }),
            )
            .await?;
            imported_tasks.push(ImportedTaskRecord {
                brehon_task_id: created["task_id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Task tool did not return task_id for phase gate"))?
                    .to_string(),
                phase_id: phase.id.clone(),
                phase_title: phase.title.clone(),
                source_epic_id: None,
                source_epic_title: None,
                task: gate_task.clone(),
                is_phase_gate: true,
            });
        }
    }

    let final_hardening = call_task_tool(
        &tool,
        json!({
            "action": "ensure_final_hardening",
            "id": initiative_id.clone(),
            "source_file": plan.path.display().to_string(),
            "role": "supervisor",
            "agent_name": "plan-importer"
        }),
    )
    .await?;
    let final_hardening_epic_id = final_hardening["epic"]["task_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Task tool did not return final hardening epic task_id"))?
        .to_string();

    let source_to_brehon: HashMap<String, String> = imported_tasks
        .iter()
        .map(|record| (record.task.source_id.clone(), record.brehon_task_id.clone()))
        .collect();

    for record in &imported_tasks {
        let dependencies = record
            .task
            .dependencies
            .iter()
            .filter_map(|dependency_id| source_to_brehon.get(dependency_id).cloned())
            .collect::<Vec<_>>();
        let mapped_status =
            map_source_status_to_brehon_status(&record.task.source_status).unwrap_or("pending");
        let mut update_args = json!({
            "action": "update",
            "id": record.brehon_task_id,
            "dependencies": dependencies,
            "role": "supervisor",
            "agent_name": "plan-importer"
        });
        if mapped_status != "pending" && mapped_status != "closed" {
            update_args["status"] = Value::String(mapped_status.to_string());
        }
        if mapped_status == "blocked"
            && record.task.dependencies.is_empty()
            && update_args.get("status").and_then(|value| value.as_str()) == Some("blocked")
        {
            update_args["blockers"] = Value::String(format!(
                "Imported source task {} is marked BLOCKED in the source plan.",
                record.task.source_id
            ));
        }
        let _ = call_task_tool(&tool, update_args).await?;

        if mapped_status == "closed" {
            seed_imported_terminal_task_state(project_root, &record.brehon_task_id)?;
        }

        patch_task_file(
            project_root,
            &record.brehon_task_id,
            json!({
                "plan_import": {
                    "source_file": plan.path.display().to_string(),
                    "phase_id": record.phase_id,
                    "phase_title": record.phase_title,
                    "source_task_id": record.task.source_id,
                    "source_epic_id": record.source_epic_id,
                    "source_epic_title": record.source_epic_title,
                    "source_size": record.task.size,
                    "source_gate": record.task.gate,
                    "source_status": record.task.source_status,
                    "details_doc": record.task.details_doc.clone(),
                    "required_reading": record.task.required_reading.clone(),
                    "context_refs": record.task.context_refs.clone(),
                    "is_phase_gate": record.is_phase_gate
                }
            }),
        )?;
    }

    println!(
        "Imported '{}' as initiative {} with {} phase epics, {} imported tasks, final hardening epic {}, and {} hardening seed tasks.",
        plan.title,
        initiative_id,
        plan.phases.len(),
        imported_tasks.len(),
        final_hardening_epic_id,
        FINAL_HARDENING_SEED_TASK_COUNT
    );

    Ok(())
}

pub async fn execute_extract(
    project_root: &Path,
    plan_path: &Path,
    output_path: Option<&Path>,
    mode: ExtractMode,
) -> Result<()> {
    ensure_git_repo(project_root)?;
    let plan = load_plan_document(project_root, plan_path, mode).await?;
    let rendered = serde_json::to_string_pretty(&plan)?;

    if let Some(output_path) = output_path {
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output_path, rendered)?;
        println!(
            "Extracted normalized plan '{}' to {}",
            plan.title,
            output_path.display()
        );
    } else {
        println!("{rendered}");
    }

    Ok(())
}
