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

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => {
                result.push(name);
            }
            std::path::Component::ParentDir => {
                if result.file_name().is_some() {
                    result.pop();
                }
            }
            std::path::Component::RootDir => {
                result = PathBuf::from("/");
            }
            std::path::Component::Prefix(prefix) => {
                result = PathBuf::from(prefix.as_os_str());
            }
            std::path::Component::CurDir => {}
        }
    }
    result
}

fn paths_equal_for_import_check(project_root: &Path, a: &Path, b: &Path) -> bool {
    let resolve = |p: &Path| -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            project_root.join(p)
        }
    };
    let a_resolved = resolve(a);
    let b_resolved = resolve(b);
    match (a_resolved.canonicalize(), b_resolved.canonicalize()) {
        (Ok(a_canon), Ok(b_canon)) => a_canon == b_canon,
        (Ok(a_canon), Err(_)) => a_canon == normalize_path(&b_resolved),
        (Err(_), Ok(b_canon)) => normalize_path(&a_resolved) == b_canon,
        (Err(_), Err(_)) => normalize_path(&a_resolved) == normalize_path(&b_resolved),
    }
}

#[derive(Debug, serde::Deserialize)]
struct TaskImportRef {
    task_id: Option<String>,
    plan_import: Option<PlanImportRef>,
}

#[derive(Debug, serde::Deserialize)]
struct PlanImportRef {
    source_file: Option<String>,
}

fn find_prior_import_of_source_file(
    project_root: &Path,
    source_path: &Path,
) -> Result<Option<(String, String)>> {
    let tasks_dir = project_root.join(".brehon").join("runtime").join("tasks");
    if !tasks_dir.exists() {
        return Ok(None);
    }
    let entries = fs::read_dir(&tasks_dir)
        .with_context(|| format!("Cannot scan tasks directory '{}'", tasks_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                eprintln!(
                    "Warning: could not read task file '{}': {err}",
                    path.display()
                );
                continue;
            }
        };
        let record: TaskImportRef = match serde_json::from_str(&content) {
            Ok(record) => record,
            Err(err) => {
                eprintln!(
                    "Warning: could not parse task JSON '{}': {err}",
                    path.display()
                );
                continue;
            }
        };
        let Some(stored_source) = record
            .plan_import
            .as_ref()
            .and_then(|p| p.source_file.as_deref())
        else {
            continue;
        };
        let stored_path = Path::new(stored_source);
        let task_id = record.task_id.unwrap_or_else(|| "unknown".to_string());

        // Legacy task files created before canonicalization may contain
        // relative paths that escape project_root (e.g. "../plan.json").
        // When that happens we cannot reliably compare the full paths,
        // so we fall back to basename comparison: if the filenames match
        // we treat it as a potential duplicate (fail-closed); if they
        // differ we skip the unrelated entry.
        if !stored_path.is_absolute() {
            let resolved = normalize_path(&project_root.join(stored_path));
            let project_root_normalized = normalize_path(project_root);
            if !resolved.starts_with(&project_root_normalized) {
                let stored_basename = stored_path.file_name();
                let source_basename = source_path.file_name();
                if stored_basename.is_some() && stored_basename == source_basename {
                    eprintln!(
                        "Warning: task {} has plan_import.source_file '{}' which escapes \
                         the project root '{}'. Basename matches the requested plan, so \
                         treating as duplicate (fail-closed).",
                        task_id,
                        stored_source,
                        project_root.display()
                    );
                    return Ok(Some((task_id, stored_source.to_string())));
                } else {
                    eprintln!(
                        "Warning: task {} has plan_import.source_file '{}' which escapes \
                         the project root '{}'. Basename does not match the requested plan; \
                         skipping unrelated entry.",
                        task_id,
                        stored_source,
                        project_root.display()
                    );
                    continue;
                }
            }
        }

        if paths_equal_for_import_check(project_root, stored_path, source_path) {
            return Ok(Some((task_id, stored_source.to_string())));
        }
    }
    Ok(None)
}

async fn git_commit_is_ancestor(project_root: &Path, commit: &str) -> Result<bool> {
    let cmd = tokio::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .current_dir(project_root)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), cmd)
        .await
        .with_context(|| format!("git merge-base --is-ancestor {commit} HEAD timed out"))??;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        other => bail!(
            "git merge-base --is-ancestor {commit} HEAD failed with exit code {other:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
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
    let mut plan = load_plan_document(project_root, plan_path, mode).await?;

    // Normalize the plan path to an absolute/canonical path so that
    // plan_import.source_file comparisons are independent of CLI spelling or CWD.
    plan.path = plan
        .path
        .canonicalize()
        .unwrap_or_else(|_| plan.path.clone());

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

    // Detection A: source file already imported
    let prior_import = tokio::task::spawn_blocking({
        let project_root = project_root.to_path_buf();
        let plan_path = plan.path.clone();
        move || find_prior_import_of_source_file(&project_root, &plan_path)
    })
    .await
    .with_context(|| "task scanning panicked")??;
    if let Some((prior_task_id, _)) = prior_import {
        bail!(
            "Cannot import '{}': this plan was already imported (see task {}). \
             A residual follow-up plan is required for additional changes.",
            plan.path.display(),
            prior_task_id
        );
    }

    // Detection B: plan-specific landed-commit guard
    if let Some(landed_commit) = plan.already_landed_commit.as_deref() {
        if git_commit_is_ancestor(project_root, landed_commit).await? {
            bail!(
                "Cannot import '{}': commit {} is already \
                 on this branch. A residual follow-up plan is required for additional changes.",
                plan.path.display(),
                landed_commit
            );
        }
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

#[cfg(test)]
mod duplicate_import_guard_tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn paths_equal_for_import_check_resolves_relative_against_project_root() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path();
        fs::write(project_root.join("plan.json"), "{}").unwrap();

        assert!(paths_equal_for_import_check(
            project_root,
            Path::new("plan.json"),
            Path::new("plan.json")
        ));
        assert!(paths_equal_for_import_check(
            project_root,
            Path::new("./plan.json"),
            Path::new("plan.json")
        ));
        assert!(paths_equal_for_import_check(
            project_root,
            Path::new("plan.json"),
            &project_root.join("plan.json")
        ));
        assert!(!paths_equal_for_import_check(
            project_root,
            Path::new("plan.json"),
            Path::new("other.json")
        ));
    }

    #[test]
    fn paths_equal_for_import_check_falls_back_when_file_removed() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path();
        // File does not exist, so canonicalize fails
        assert!(paths_equal_for_import_check(
            project_root,
            Path::new("plan.json"),
            Path::new("plan.json")
        ));
        assert!(!paths_equal_for_import_check(
            project_root,
            Path::new("plan.json"),
            Path::new("other.json")
        ));
    }

    #[test]
    fn paths_equal_for_import_check_falls_back_with_redundant_components() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path();
        // Neither file exists, so canonicalize fails on both sides and we hit
        // the (Err, Err) arm. The paths differ only by redundant ../
        // components, so normalize_path must make them equal.
        assert!(paths_equal_for_import_check(
            project_root,
            Path::new("nonexistent/../plan.json"),
            Path::new("plan.json")
        ));
        assert!(!paths_equal_for_import_check(
            project_root,
            Path::new("nonexistent/../plan.json"),
            Path::new("other.json")
        ));
        // Also cover redundant ./ components (CurDir) in the (Err, Err) arm.
        assert!(paths_equal_for_import_check(
            project_root,
            Path::new("./plan.json"),
            Path::new("plan.json")
        ));
    }

    #[test]
    fn paths_equal_for_import_check_mixed_canonicalize_fallback() {
        let dir = TempDir::new().unwrap();
        // Canonicalize project_root so that canonicalize() on existing files under it
        // produces paths with the same prefix as normalize_path() on non-existent paths.
        // (On macOS, /var is a symlink to /private/var; canonicalize resolves it.)
        let project_root = dir.path().canonicalize().unwrap();
        fs::write(project_root.join("plan.json"), "{}").unwrap();
        // Create a subdir so that dir/../plan.json logically resolves to plan.json,
        // but canonicalize fails because dir does not exist.
        // a (existing file) canonicalizes; b (via nonexistent dir) does not.
        assert!(paths_equal_for_import_check(
            &project_root,
            Path::new("plan.json"),
            Path::new("nonexistent/../plan.json")
        ));
        assert!(!paths_equal_for_import_check(
            &project_root,
            Path::new("plan.json"),
            Path::new("nonexistent/../other.json")
        ));
    }

    #[test]
    fn find_prior_import_of_source_file_missing_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_prior_import_of_source_file_empty_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".brehon").join("runtime").join("tasks")).unwrap();
        let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_prior_import_of_source_file_skips_malformed_json() {
        let dir = TempDir::new().unwrap();
        let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(tasks_dir.join("T-bad.json"), "not json").unwrap();
        fs::write(
            tasks_dir.join("T-good.json"),
            r#"{"task_id":"T-good","plan_import":{"source_file":"plan.json"}}"#,
        )
        .unwrap();
        let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
        assert_eq!(
            result,
            Some(("T-good".to_string(), "plan.json".to_string()))
        );
    }

    #[test]
    fn find_prior_import_of_source_file_skips_task_without_plan_import() {
        let dir = TempDir::new().unwrap();
        let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("T-no-import.json"),
            r#"{"task_id":"T-no-import"}"#,
        )
        .unwrap();
        let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn git_commit_is_ancestor_fails_for_invalid_ref() {
        let dir = TempDir::new().unwrap();
        // Initialize a minimal repo
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        fs::write(dir.path().join("a"), "a\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "a"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(dir.path())
            .output()
            .unwrap();

        let err = git_commit_is_ancestor(dir.path(), "deadbeef")
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("deadbeef"),
            "expected deadbeef error, got: {message}"
        );
    }

    #[tokio::test]
    async fn git_commit_is_ancestor_returns_false_for_real_non_ancestor() {
        let dir = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        fs::write(dir.path().join("a"), "a\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "a"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "first"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(dir.path())
            .output()
            .unwrap();
        fs::write(dir.path().join("b"), "b\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "b"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "second"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(dir.path())
            .output()
            .unwrap();

        // Capture the second commit hash, then checkout the first commit.
        // The second commit is NOT an ancestor of the first commit.
        let second_commit = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        std::process::Command::new("git")
            .args(["checkout", "HEAD~1"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        assert!(!git_commit_is_ancestor(dir.path(), &second_commit)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn git_commit_is_ancestor_returns_true_for_actual_ancestor() {
        let dir = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        fs::write(dir.path().join("a"), "a\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "a"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "first"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(dir.path())
            .output()
            .unwrap();
        fs::write(dir.path().join("b"), "b\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "b"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "second"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(dir.path())
            .output()
            .unwrap();

        assert!(git_commit_is_ancestor(dir.path(), "HEAD~1").await.unwrap());
    }

    #[test]
    fn find_prior_import_fail_closed_when_legacy_basename_matches() {
        let dir = TempDir::new().unwrap();
        let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        // Legacy task file from a subdirectory import before canonicalization.
        // The relative path escapes project_root, but the basename matches the
        // requested plan, so we fail-closed and treat it as a duplicate.
        fs::write(
            tasks_dir.join("T-legacy.json"),
            r#"{"task_id":"T-legacy","plan_import":{"source_file":"../plan.json"}}"#,
        )
        .unwrap();
        let source_path = dir.path().join("plan.json");
        let result = find_prior_import_of_source_file(dir.path(), &source_path).unwrap();
        assert!(
            result.is_some(),
            "legacy path ../plan.json with matching basename should be treated as duplicate"
        );
    }

    #[test]
    fn find_prior_import_fail_closed_when_external_legacy_basename_matches() {
        let dir = TempDir::new().unwrap();
        let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        // Old task file with an escaping relative path. The basename matches
        // the in-repo plan we want to import, so we fail-closed.
        fs::write(
            tasks_dir.join("T-external.json"),
            r#"{"task_id":"T-external","plan_import":{"source_file":"../outside/plan.json"}}"#,
        )
        .unwrap();
        let source_path = dir.path().join("outside").join("plan.json");
        let result = find_prior_import_of_source_file(dir.path(), &source_path).unwrap();
        assert!(
            result.is_some(),
            "legacy path ../outside/plan.json with matching basename should be treated as duplicate"
        );
    }

    #[test]
    fn find_prior_import_skips_legacy_relative_source_file_when_basename_differs() {
        let dir = TempDir::new().unwrap();
        let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        // Legacy task file with an escaping relative path. The basename does
        // NOT match the requested plan, so we skip the unrelated entry.
        fs::write(
            tasks_dir.join("T-legacy.json"),
            r#"{"task_id":"T-legacy","plan_import":{"source_file":"../plan-a.json"}}"#,
        )
        .unwrap();
        let source_path = dir.path().join("plan-b.json");
        let result = find_prior_import_of_source_file(dir.path(), &source_path).unwrap();
        assert!(
            result.is_none(),
            "legacy path ../plan-a.json with differing basename should be skipped"
        );
    }

    #[test]
    fn find_prior_import_warns_on_unreadable_task_file() {
        let dir = TempDir::new().unwrap();
        let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("T-good.json"),
            r#"{"task_id":"T-good","plan_import":{"source_file":"plan.json"}}"#,
        )
        .unwrap();
        // Create an unreadable file (permissions 000)
        fs::write(
            tasks_dir.join("T-unreadable.json"),
            r#"{"task_id":"T-unreadable","plan_import":{"source_file":"plan.json"}}"#,
        )
        .unwrap();
        let mut perms = fs::metadata(tasks_dir.join("T-unreadable.json"))
            .unwrap()
            .permissions();
        perms.set_mode(0o000);
        fs::set_permissions(tasks_dir.join("T-unreadable.json"), perms).unwrap();

        let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
        assert_eq!(
            result,
            Some(("T-good".to_string(), "plan.json".to_string()))
        );

        // Restore permissions so tempdir cleanup doesn't fail
        let mut perms = fs::metadata(tasks_dir.join("T-unreadable.json"))
            .unwrap()
            .permissions();
        perms.set_mode(0o644);
        fs::set_permissions(tasks_dir.join("T-unreadable.json"), perms).unwrap();
    }
}
