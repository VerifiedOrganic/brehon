use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path};

use anyhow::{anyhow, bail, Context, Result};

use super::types::*;

pub(crate) fn slugify_branch_component(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if ch.is_ascii_whitespace() || matches!(ch, '-' | '_' | '/' | ':') {
            Some('-')
        } else {
            None
        };
        let Some(normalized) = normalized else {
            continue;
        };
        if normalized == '-' {
            if slug.is_empty() || last_was_dash {
                continue;
            }
            slug.push('-');
            last_was_dash = true;
        } else {
            slug.push(normalized);
            last_was_dash = false;
        }
    }
    slug.trim_matches('-').to_string()
}

pub(crate) fn phase_integration_branch(phase: &PlanPhase, initiative_id: &str) -> String {
    let initiative_suffix = initiative_id
        .strip_prefix("T-")
        .unwrap_or(initiative_id)
        .to_ascii_lowercase();
    let slug = slugify_branch_component(&format!("phase-{}-{}", phase.id, phase.title));
    if slug.is_empty() {
        format!("epic/{initiative_suffix}")
    } else {
        format!("epic/{slug}-{initiative_suffix}")
    }
}

fn parse_heading_title(line: &str, prefix: &str) -> Option<String> {
    line.trim()
        .strip_prefix(prefix)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn parse_phase_heading(line: &str) -> Option<(String, String)> {
    let raw = parse_heading_title(line, "## Phase ")?;
    let (id, title) = raw.split_once(':')?;
    Some((id.trim().to_string(), title.trim().to_string()))
}

pub(crate) fn parse_chunkable_phase_heading(line: &str) -> Option<(String, String)> {
    let raw = parse_heading_title(line, "## ")?;
    let (_, remainder) = raw.split_once("Phase ")?;
    let remainder = remainder.trim();

    let mut id_end = 0usize;
    for (index, ch) in remainder.char_indices() {
        if ch.is_ascii_digit() || ch == '.' {
            id_end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if id_end == 0 {
        return None;
    }

    let id = remainder[..id_end].trim();
    let title = remainder[id_end..]
        .trim_start_matches(|ch: char| {
            ch.is_ascii_whitespace() || matches!(ch, ':' | '—' | '-' | '_')
        })
        .trim();
    if title.is_empty() {
        return None;
    }

    Some((id.to_string(), title.to_string()))
}

fn parse_epic_heading(line: &str) -> Option<(String, String)> {
    let raw = parse_heading_title(line, "### Epic ")?;
    let (id, title) = raw.split_once(':')?;
    Some((id.trim().to_string(), title.trim().to_string()))
}

fn parse_phase_gate_heading(line: &str) -> Option<String> {
    let raw = parse_heading_title(line, "### Phase ")?;
    raw.strip_suffix(" Gate")
        .map(|value| value.trim().to_string())
}

pub(crate) fn parse_task_extraction_heading(line: &str) -> Option<(String, String)> {
    let raw = parse_heading_title(line, "### ")?;
    let (source_id, title) = raw.split_once(' ')?;
    if !source_id.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((source_id.trim().to_string(), title.trim().to_string()))
}

fn split_markdown_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return None;
    }
    let cells = trimmed[1..trimmed.len() - 1]
        .split('|')
        .map(|cell| cell.trim().trim_matches('`').to_string())
        .collect::<Vec<_>>();
    Some(cells)
}

fn parse_dependencies_cell(cell: &str) -> Vec<String> {
    let trimmed = cell.trim().trim_matches('`');
    if trimmed.is_empty() || trimmed == "—" || trimmed == "-" {
        Vec::new()
    } else {
        trimmed
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
            .collect()
    }
}

fn parse_table(lines: &[&str], start: usize) -> Result<(Vec<PlanTask>, usize)> {
    let mut index = start;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    if index >= lines.len() {
        bail!("Expected markdown table after heading, found EOF");
    }
    if !lines[index].trim().starts_with("| ID |") {
        bail!(
            "Expected markdown table header after heading, found '{}'",
            lines[index].trim()
        );
    }
    index += 2;

    let mut tasks = Vec::new();
    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() {
            index += 1;
            break;
        }
        if !line.starts_with('|') {
            break;
        }
        let cells = split_markdown_row(line)
            .ok_or_else(|| anyhow!("Malformed markdown table row: {line}"))?;
        if cells.len() < 6 {
            bail!(
                "Expected 6 columns in markdown table row, found {}: {}",
                cells.len(),
                line
            );
        }
        tasks.push(PlanTask {
            source_id: cells[0].clone(),
            title: cells[1].clone(),
            dependencies: parse_dependencies_cell(&cells[2]),
            size: cells[3].clone(),
            gate: cells[4].clone(),
            source_status: cells[5].clone(),
            details_doc: None,
            required_reading: Vec::new(),
            context_refs: Vec::new(),
        });
        index += 1;
    }

    Ok((tasks, index))
}

pub(crate) fn parse_document(path: &Path) -> Result<PlanDocument> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read plan file '{}'", path.display()))?;
    let lines: Vec<&str> = content.lines().collect();

    let title = lines
        .iter()
        .find_map(|line| parse_heading_title(line, "# "))
        .ok_or_else(|| anyhow!("Plan file is missing a top-level '# ' title"))?;

    let project = lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("**Project:**")
            .map(|value| value.trim().to_string())
    });
    let stack = lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("**Stack:**")
            .map(|value| value.trim().to_string())
    });
    let target = lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("**Target:**")
            .map(|value| value.trim().to_string())
    });

    let mut phases = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some((phase_id, phase_title)) = parse_phase_heading(lines[index]) else {
            index += 1;
            continue;
        };
        let mut phase = PlanPhase {
            id: phase_id,
            title: phase_title,
            notes: Vec::new(),
            epics: Vec::new(),
            gate_task: None,
        };
        index += 1;

        while index < lines.len() {
            let line = lines[index].trim();
            if line.starts_with("## Phase ") || line == "## Cross-Phase Dependency Summary" {
                break;
            }

            if let Some((epic_id, epic_title)) = parse_epic_heading(line) {
                let (tasks, next_index) = parse_table(&lines, index + 1)?;
                phase.epics.push(PlanEpic {
                    source_id: epic_id,
                    title: epic_title,
                    tasks,
                });
                index = next_index;
                continue;
            }

            if parse_phase_gate_heading(line).is_some() {
                let (tasks, next_index) = parse_table(&lines, index + 1)?;
                if tasks.len() != 1 {
                    bail!(
                        "Expected exactly one task row in phase gate table for phase {}",
                        phase.id
                    );
                }
                phase.gate_task = tasks.into_iter().next();
                index = next_index;
                continue;
            }

            if let Some(note) = line.strip_prefix('>') {
                let note = note.trim();
                if !note.is_empty() {
                    phase.notes.push(note.to_string());
                }
            }

            index += 1;
        }

        phases.push(phase);
    }

    if phases.is_empty() {
        bail!("No phase sections found in '{}'", path.display());
    }

    Ok(PlanDocument {
        title,
        project,
        stack,
        target,
        path: path.to_path_buf(),
        phases,
    })
}

pub(crate) fn parse_chunkable_plan_document(
    path: &Path,
    content: &str,
) -> Result<Option<ChunkablePlanDocument>> {
    let lines: Vec<&str> = content.lines().collect();
    let title = match lines
        .iter()
        .find_map(|line| parse_heading_title(line, "# "))
    {
        Some(title) => title,
        None => return Ok(None),
    };

    let project = lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("**Project:**")
            .map(|value| value.trim().to_string())
    });
    let stack = lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("**Stack:**")
            .map(|value| value.trim().to_string())
    });
    let target = lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("**Target:**")
            .map(|value| value.trim().to_string())
    });

    let mut status_context = None;
    if let Some(status_index) = lines.iter().position(|line| line.trim() == "## Status") {
        let mut buffer = Vec::new();
        let mut index = status_index;
        while index < lines.len() {
            let line = lines[index];
            if index > status_index && line.starts_with("## ") {
                break;
            }
            buffer.push(line);
            index += 1;
        }
        let rendered = buffer.join("\n").trim().to_string();
        if !rendered.is_empty() {
            status_context = Some(rendered);
        }
    }

    let mut phases = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        let Some((phase_id, phase_title)) = parse_chunkable_phase_heading(line) else {
            index += 1;
            continue;
        };
        let heading = lines[index].to_string();
        index += 1;
        let start = index;
        while index < lines.len() {
            if lines[index].starts_with("## ") {
                break;
            }
            index += 1;
        }
        let body = lines[start..index].join("\n").trim().to_string();
        phases.push(PhaseExtractionSection {
            id: phase_id,
            title: phase_title,
            heading,
            body,
        });
    }

    if phases.is_empty() {
        return Ok(None);
    }

    Ok(Some(ChunkablePlanDocument {
        title,
        project,
        stack,
        target,
        path: path.to_path_buf(),
        status_context,
        phases,
    }))
}

pub(crate) fn parse_task_extraction_sections(
    phase: &PhaseExtractionSection,
) -> Vec<TaskExtractionSection> {
    let lines: Vec<&str> = phase.body.lines().collect();
    let mut sections = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        let Some((source_id, title)) = parse_task_extraction_heading(line) else {
            index += 1;
            continue;
        };
        let heading = lines[index].to_string();
        index += 1;
        let start = index;
        while index < lines.len() {
            if lines[index].starts_with("### ") {
                break;
            }
            index += 1;
        }
        sections.push(TaskExtractionSection {
            source_id,
            title,
            heading,
            body: lines[start..index].join("\n").trim().to_string(),
        });
    }
    sections
}

pub(crate) fn normalize_extracted_metadata_text(text: &str) -> String {
    text.replace('`', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn normalize_extracted_phase_id_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let lower = trimmed.to_ascii_lowercase();
    let remainder = lower
        .strip_prefix("phase")
        .map(|value| {
            value.trim_start_matches(|ch: char| {
                ch.is_ascii_whitespace() || matches!(ch, '-' | '_' | ':')
            })
        })
        .unwrap_or(lower.as_str());

    remainder
        .trim()
        .trim_start_matches(|ch: char| matches!(ch, '-' | '_' | ':'))
        .to_string()
}

pub(crate) fn strip_redundant_phase_prefix(text: &str) -> String {
    let trimmed = text.trim();
    let Some(remainder) = trimmed.strip_prefix("Phase ") else {
        return trimmed.to_string();
    };

    for delimiter in ["—", ":"] {
        if let Some((phase_id, title)) = remainder.split_once(delimiter) {
            let phase_id = phase_id.trim();
            let title = title.trim();
            if !title.is_empty()
                && phase_id.chars().any(|ch| ch.is_ascii_digit())
                && phase_id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch.is_ascii_whitespace())
            {
                return title.to_string();
            }
        }
    }

    trimmed.to_string()
}

pub(crate) fn extracted_metadata_matches(expected: &str, actual: &str) -> bool {
    let expected = normalize_extracted_metadata_text(expected);
    let actual = normalize_extracted_metadata_text(actual);
    expected == actual
        || normalize_extracted_metadata_text(&strip_redundant_phase_prefix(&expected))
            == normalize_extracted_metadata_text(&strip_redundant_phase_prefix(&actual))
}

pub(crate) fn extracted_phase_id_matches(expected: &str, actual: &str) -> bool {
    normalize_extracted_phase_id_text(expected) == normalize_extracted_phase_id_text(actual)
}

pub(crate) fn plan_tasks<'a>(
    phase: &'a PlanPhase,
) -> impl Iterator<Item = (Option<&'a PlanEpic>, &'a PlanTask)> + 'a {
    phase
        .epics
        .iter()
        .flat_map(|epic| epic.tasks.iter().map(move |task| (Some(epic), task)))
        .chain(phase.gate_task.iter().map(|task| (None, task)))
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn validate_path_reference(field: &str, task_id: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field} for task {task_id} must not be empty");
    }
    if trimmed.contains("://") {
        bail!("{field} for task {task_id} must be a filesystem path, not a URL");
    }
    let path = Path::new(trimmed);
    if has_parent_component(path) {
        bail!("{field} for task {task_id} must not contain parent traversal");
    }
    Ok(())
}

fn validate_details_doc(task: &PlanTask) -> Result<()> {
    let Some(details_doc) = task.details_doc.as_deref() else {
        return Ok(());
    };
    validate_path_reference("details_doc", &task.source_id, details_doc)?;
    let path = Path::new(details_doc.trim());
    let is_markdown = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
    if !is_markdown {
        bail!(
            "details_doc for task {} must be a .md file path",
            task.source_id
        );
    }
    Ok(())
}

fn validate_reference_list(field: &str, task: &PlanTask, values: &[String]) -> Result<()> {
    for value in values {
        validate_path_reference(field, &task.source_id, value)?;
    }
    Ok(())
}

pub(crate) fn validate_plan_document(
    plan: &PlanDocument,
    source_path: &Path,
) -> Result<PlanDocument> {
    if plan.title.trim().is_empty() {
        bail!("Imported plan is missing a title");
    }
    if plan.phases.is_empty() {
        bail!("Imported plan '{}' has no phases", plan.title);
    }

    let mut normalized = plan.clone();
    if normalized.path.as_os_str().is_empty() {
        normalized.path = source_path.to_path_buf();
    }

    let mut phase_ids = HashSet::new();
    let mut source_task_ids = HashMap::new();
    for phase in &normalized.phases {
        if phase.id.trim().is_empty() || phase.title.trim().is_empty() {
            bail!("Each phase must have a non-empty id and title");
        }
        if !phase_ids.insert(phase.id.clone()) {
            bail!("Duplicate phase id '{}' in extracted plan", phase.id);
        }
        if phase.epics.is_empty() && phase.gate_task.is_none() {
            bail!(
                "Phase {} ({}) has no epics or gate task in extracted plan",
                phase.id,
                phase.title
            );
        }

        let mut phase_epic_ids = HashSet::new();
        for epic in &phase.epics {
            if epic.source_id.trim().is_empty() || epic.title.trim().is_empty() {
                bail!(
                    "Every source epic in phase {} must have a non-empty source_id and title",
                    phase.id
                );
            }
            if !phase_epic_ids.insert(epic.source_id.clone()) {
                bail!(
                    "Duplicate source epic id '{}' inside phase {}",
                    epic.source_id,
                    phase.id
                );
            }
            if epic.tasks.is_empty() {
                bail!(
                    "Source epic {} ({}) in phase {} contains no tasks",
                    epic.source_id,
                    epic.title,
                    phase.id
                );
            }
        }

        for (_, task) in plan_tasks(phase) {
            if task.source_id.trim().is_empty() || task.title.trim().is_empty() {
                bail!("Each imported task must have a non-empty source_id and title");
            }
            if let Some(existing) = source_task_ids.insert(task.source_id.clone(), phase.id.clone())
            {
                bail!(
                    "Duplicate source task id '{}' appears in phases {} and {}",
                    task.source_id,
                    existing,
                    phase.id
                );
            }
            if !task.source_status.trim().is_empty()
                && map_source_status_to_brehon_status(&task.source_status).is_none()
            {
                bail!(
                    "Unsupported source status '{}' for task {}",
                    task.source_status,
                    task.source_id
                );
            }
            validate_details_doc(task)?;
            validate_reference_list("required_reading", task, &task.required_reading)?;
            validate_reference_list("context_refs", task, &task.context_refs)?;
        }
    }

    for phase in &normalized.phases {
        for (_, task) in plan_tasks(phase) {
            for dependency in &task.dependencies {
                if !source_task_ids.contains_key(dependency) {
                    bail!(
                        "Task {} depends on unknown source task '{}'",
                        task.source_id,
                        dependency
                    );
                }
            }
        }
    }

    Ok(normalized)
}

pub(crate) fn parse_normalized_plan(path: &Path) -> Result<PlanDocument> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read normalized plan '{}'", path.display()))?;
    let mut plan: PlanDocument = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse normalized plan JSON '{}'", path.display()))?;
    if plan.path.as_os_str().is_empty() {
        plan.path = path.to_path_buf();
    }
    validate_plan_document(&plan, path)
}

pub(crate) fn map_source_status_to_brehon_status(source_status: &str) -> Option<&'static str> {
    match source_status.trim().trim_matches('`') {
        "READY" => Some("pending"),
        "BLOCKED" => Some("blocked"),
        "IN_PROGRESS" => Some("pending"),
        "DONE" => Some("closed"),
        "FAILED" => Some("blocked"),
        _ => None,
    }
}
